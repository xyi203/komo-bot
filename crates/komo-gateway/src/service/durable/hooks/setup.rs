//! 这个 conversation 是谁、用哪个模型、在哪个目录、预算多少（`AgentHooks::setup` 的前半）。
//!
//! 解析顺序（`docs/storage-durable-d5.md` §6）：**`komo.agent`**（conversation 自己的）→
//! **`komo.task`**（任务会话，沿 `parent` 回退到父的 `komo.agent`）→ **默认 Agent**。每次
//! 按**当前**配置快照解析（热重载）：Agent 从 `[agents]` 删掉了就退回默认 Agent（一条配置改动
//! 不该让输入失败，同旧 `freeze_run`），模型 alias 按当前 `model_catalog` 找。
//!
//! 模型的优先级：
//!
//! - 对话 conversation：`komo.agent.model`（sticky，`/model`、`submit` 带的）→ Profile 的 alias →
//!   主模型；`komo.agent.effort` 盖在上面。
//! - 任务会话：`komo.task` 给的名字**优先**（`task` 工具带的 `model` / `effort`）；没给 model 时，
//!   fresh 任务用 Agent 的默认模型，fork 任务沿用父此刻解析出的模型与 effort（缓存才命中）。
//! - 写进 document 的 alias 在当前配置里找不到（配置改过）= 没有可用的模型：`NoModel`，这一轮的
//!   输入以 `no_model` 收场并说清是哪个 alias——同旧 `submit` 对"当前分支的模型不在配置中"的
//!   拒绝；只有 *Profile* 指的 alias 写错时退回主模型（同旧 `agent_model`，一次笔误不该丢输入）。

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use komo_kernel::durable::{ConversationId, Storage};
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::types::agent::AgentProfile;
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::model::{Effort, ModelConfig};
use komo_runtime::agent::RetryBudget;
use komo_runtime::durable::harness::{HookError, RetryPolicy, RunBudget, TaskDoc, task_doc};
use komo_runtime::tools::paths;

use crate::service::durable::agent_doc::{self, AgentDoc};
use crate::service::state::GatewayState;

/// 任务链最多往上找几层（`komo.task.parent`；任务里不能再派任务，所以正常只有一层）。
const MAX_DEPTH: usize = 8;

/// 一个 conversation 此刻的解析结果。
#[derive(Debug, Clone)]
pub(crate) struct Resolved {
    pub conversation: ConversationId,
    /// 工具输出与 blob 落在哪个 Session 目录。
    pub session: SessionId,
    pub agent_id: String,
    pub profile: AgentProfile,
    /// 生效的 `komo.agent`：自己的，没有就是（任务会话）父的；都没有 = 默认 Agent。
    pub doc: Option<AgentDoc>,
    /// 有 = 任务会话。
    pub task: Option<TaskDoc>,
    pub model: ModelConfig,
    pub workspace: PathBuf,
    pub budget: RunBudget,
    /// 提示缓存的路由键：Session ID；fork 任务用父的。
    pub cache_key: String,
}

impl Resolved {
    pub fn is_task(&self) -> bool {
        self.task.is_some()
    }

    pub fn is_fork(&self) -> bool {
        self.task.as_ref().is_some_and(|task| task.fork)
    }

    /// 对话会话里直接受理的输入（不是任务会话、也不是 cron）：`bash` 才转后台、禁长 sleep。
    pub fn is_conversation(&self) -> bool {
        // W0a 合并后: 读 sessions 行的 `kind`（`main` / `normal` / `task` / `cron`），不再靠 `source`。
        self.task.is_none()
            && !self
                .doc
                .as_ref()
                .and_then(|doc| doc.source.as_deref())
                .is_some_and(|source| source.starts_with("cron:"))
    }
}

type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 解析 `conversation`。**只读已提交状态**（读不经过提交线）。
pub(crate) fn resolve<'a>(
    state: &'a GatewayState,
    storage: &'a dyn Storage,
    conversation: ConversationId,
    config: &'a ConfigSnapshot,
) -> Boxed<'a, Result<Resolved, HookError>> {
    Box::pin(resolve_at(state, storage, conversation, config, 0))
}

fn resolve_at<'a>(
    state: &'a GatewayState,
    storage: &'a dyn Storage,
    conversation: ConversationId,
    config: &'a ConfigSnapshot,
    depth: usize,
) -> Boxed<'a, Result<Resolved, HookError>> {
    Box::pin(async move {
        if depth > MAX_DEPTH {
            return Err(HookError::Failed(format!(
                "conversation {conversation} 的任务链超过 {MAX_DEPTH} 层，解析不出它的 Agent"
            )));
        }
        let own = agent_doc::read(storage, conversation)
            .await
            .map_err(|e| HookError::Failed(format!("读 komo.agent 失败：{e}")))?;
        let task = task_doc(storage, conversation)
            .await
            .map_err(|e| HookError::Failed(format!("读 komo.task 失败：{e}")))?;
        let parent = match &task {
            Some(task) => Some(Box::new(
                resolve_at(state, storage, task.parent, config, depth + 1).await?,
            )),
            None => None,
        };
        let doc = own.or_else(|| parent.as_ref().and_then(|parent| parent.doc.clone()));

        let default_agent = config.agent.default_agent.as_str();
        let wanted = doc.as_ref().map_or(default_agent, |doc| doc.agent.as_str());
        let profile = match config.agent.get(wanted) {
            Some(profile) => profile.clone(),
            None => {
                // 配置里已经没有这个 Agent 了：退回默认 Agent（§八：旧归属归入默认 Agent）。
                let fallback = config.agent.default_profile();
                tracing::warn!(
                    agent = %wanted,
                    fallback = %fallback.id,
                    %conversation,
                    "这个 conversation 绑的 Agent 已经不在配置里，按默认 Agent 跑"
                );
                fallback.clone()
            }
        };

        // 工作目录：`komo.agent.workdir`（操作者对这一段对话的显式选择）优先，其次 Profile 的
        // `workspace`，最后 `[paths] workspaces_dir`。解析成真实路径（符号链接这一刻解掉）。
        let workspace = doc
            .as_ref()
            .and_then(|doc| doc.workdir.clone())
            .map(PathBuf::from)
            .or_else(|| profile.workspace.clone())
            .unwrap_or_else(|| config.paths.workspaces_dir.clone());
        let workspace = paths::real_root(&workspace);

        let mut model = match (&task, &parent) {
            // fork：沿用父此刻的模型与 effort（缓存才命中）。
            (Some(task), Some(parent)) if task.fork => parent.model.clone(),
            // fresh 任务：Agent 的默认模型。
            (Some(_), _) => profile_model(state, config, &profile),
            // 对话：sticky 的 `komo.agent.model`，没有就 Profile 的。
            (None, _) => match doc.as_ref().and_then(|doc| doc.model.as_deref()) {
                Some(alias) => catalog_model(config, alias)?,
                None => profile_model(state, config, &profile),
            },
        };
        match &task {
            Some(task) => {
                // `task` 给的名字优先；只给 effort 时模型沿用，档位换掉。
                if let Some(alias) = task.model.as_deref() {
                    model = catalog_model(config, alias)?;
                }
                if let Some(effort) = &task.effort {
                    model.effort = Some(effort.clone());
                }
            }
            None => {
                if let Some(effort) = doc.as_ref().and_then(|doc| doc.effort.as_deref()) {
                    model.effort = effort_of(effort);
                }
            }
        }
        // 配置没写窗口时用探到的；探测还没回来就是 None。
        model.context_window = state.windows.effective(&model);

        let own_session = task.is_some();
        let session = session_of(storage, conversation, own_session).await?;
        let cache_key = match (&task, &parent) {
            (Some(task), Some(parent)) if task.fork => parent.session.to_string(),
            _ => session.to_string(),
        };

        // 总轮数：cron 会话的 `maxRounds` 在 `komo.agent` 里；任务会话按全局。
        let max_rounds = match &task {
            Some(_) => state.max_rounds,
            None => doc
                .as_ref()
                .and_then(|doc| doc.max_rounds)
                .or(state.max_rounds),
        };
        let budget = RunBudget {
            max_rounds,
            max_tokens: None,
            retry: RetryPolicy::from(&RetryBudget {
                max_attempts: state.max_retries,
                ..RetryBudget::default()
            }),
        };

        Ok(Resolved {
            conversation,
            session,
            agent_id: profile.id.clone(),
            profile,
            doc,
            task,
            model,
            workspace,
            budget,
            cache_key,
        })
    })
}

/// conversation 属于哪个 Session 目录。
///
/// W0a 合并后: 读 `ConversationRecord.session`（任务子 conversation 在派出的提交里有自己的
/// sessions 行）。合并前的替身：对话 conversation 沿 fork 的 `parent` 走到根，用根的 ID；任务
/// 会话就是它自己。
async fn session_of(
    storage: &dyn Storage,
    conversation: ConversationId,
    own_session: bool,
) -> Result<SessionId, HookError> {
    let mut root = conversation;
    if !own_session {
        for _ in 0..64 {
            let record = storage
                .conversation(root)
                .await
                .map_err(|e| HookError::Failed(format!("读 conversation {root} 失败：{e}")))?;
            match record.and_then(|record| record.parent) {
                Some(point) => root = point.conversation,
                None => break,
            }
        }
    }
    Ok(SessionId::from_raw(format!("c{root}")))
}

/// 配置里的 effort 字符串：`off` = 不带 effort。
pub(super) fn effort_of(text: &str) -> Option<Effort> {
    match text.trim() {
        "" | "off" => None,
        other => Some(Effort::new(other)),
    }
}

/// document 里写的 alias：必须在当前 `model_catalog` 里，找不到就是没有可用的模型。
pub(crate) fn catalog_model(
    config: &ConfigSnapshot,
    alias: &str,
) -> Result<ModelConfig, HookError> {
    config
        .model_catalog
        .completion(alias)
        .cloned()
        .ok_or_else(|| {
            let aliases = config
                .model_catalog
                .completions()
                .map(|(alias, _)| alias)
                .collect::<Vec<_>>()
                .join("、");
            HookError::NoModel(format!(
                "模型 `{alias}` 不在当前配置的 model_catalog 里；可选值：{aliases}"
            ))
        })
}

/// Profile 指的那个模型 alias，解析不出来就用主模型（同旧 `agent_model`）。
///
/// **不因为一个写错的 alias 让输入失败**：那会把一次配置笔误变成一条丢掉的输入。解析不出来时
/// 说清楚是哪个 alias，然后按主模型跑。
pub(crate) fn profile_model(
    state: &GatewayState,
    config: &ConfigSnapshot,
    profile: &AgentProfile,
) -> ModelConfig {
    let Some(alias) = profile.model.as_deref() else {
        return config.model.clone();
    };
    match config.model_catalog.completion(alias) {
        Some(model) => model.clone(),
        None => {
            tracing::warn!(
                file = %crate::service::agent_config_file(&state.config).display(),
                agent = %profile.id,
                alias,
                "`[agents]` 指的模型 alias 不在 model_catalog 里，这一次用主模型"
            );
            config.model.clone()
        }
    }
}
