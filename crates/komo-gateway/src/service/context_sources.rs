//! 为一次 Context 装配**取数**（`docs/agent.md` §7）：这里只回答"事实从哪来"，不回答
//! "模型该看见什么"——那是 `komo-agent::context` 的事。函数都写成**自由函数、参数显式
//! 传入**，不写成 `impl GatewaySegments`：这样每个函数要用哪些存储一眼看得出来，也不用
//! 造一整个 `GatewaySegments` 就能单测。目前只有一个实现，不为此抽 trait。
//!
//! 搬到这里的（§7）：
//!
//! - [`Identity`] / [`identity_for`] / [`ambient_identity`]（冻结身份与兜底身份，§4.3）
//! - [`recall_for`] / [`recall_scopes`]（记忆召回，§9.2、§9.4）
//! - [`message_text`] / [`resolve_history`]（history 的 I/O 那一半：外置正文、
//!   `output.json`，§8.3）
//!
//! 留在 `segment.rs` 的是"为执行取数"的那一半：`resumed` / `call_request`
//! （续跑位置）、`run_roots` / `ResourceMounts`（`CallEnv` 的根与
//! 挂载）、`halt_if_corrupt`（失败收场）、`without_delegate`（能力收窄）——它们改的是
//! 执行环境，不是"模型看见什么"。

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use komo_agent::context::history::{
    Entry, EntryKind, ResolvedMessage, StoredOutput, latest_user_text,
};
use komo_agent::skills::SkillRegistry;
use komo_kernel::events::{Event, EventPayload, MemoryInjected};
use komo_kernel::fold::{Surface, SurfaceMessage};
use komo_kernel::projection::ProjectionContext;
use komo_kernel::traits::{Ledger, LedgerError, ToolOutputStore};
use komo_kernel::types::agent::{ForkOrigin, MemoryMode, ProjectFile, RunSnapshot};
use komo_kernel::types::delegate::DelegateContract;
use komo_kernel::types::ids::RunId;
use komo_kernel::types::memory::{Injection, MemoryScope};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::surface::AgentSurface;
use komo_kernel::types::tool::ToolDefinition;
use komo_kernel::types::tool::ToolImage;
use komo_kernel::types::turn::ForkPrefix;
use komo_runtime::config::ConfigHolder;
use komo_runtime::memory::MemoryManager;
use komo_runtime::tools::paths;
use komo_store::db::store_to_ledger;
use komo_store::{CheckpointStore, PayloadStore};

/// 一段执行要用到的**身份与能力**（§4.3）。
///
/// 它要么来自 `run.accepted` 里冻结的那一份，要么（旧行 / Cron / 没有快照的子 Run）由
/// 当前配置的默认 Agent 兜底算出来——见 [`identity_for`]。这里的每一格都进这一段的装配：
/// 提示、Schema、工作目录、记忆作用域。
pub(crate) struct Identity {
    /// 这一段的 Agent（进日志；判据是下面那几样）。
    pub agent_id: String,
    /// 冻结下来的模型；`None` = 兜底路径，用 `runs.model_snapshot` 那一份。
    pub model: Option<ModelConfig>,
    /// 这次允许调用的工具。**Schema 与执行器查找用的是同一个它**（§4 末）。
    pub surface: AgentSurface,
    /// 解析过的真实工作目录。
    pub workspace: PathBuf,
    /// 身份指令正文（按引用读回来的那一份）。
    pub instructions: Option<String>,
    /// 项目规范文件（`AGENTS.md` 等），外层在前。冻结路径按引用读回；兜底路径受理时
    /// 没找过，现场找一遍（见 [`ambient_identity`]）。
    pub project_files: Vec<ProjectFile>,
    /// 受理时现场找到、与 `project_files` 不一样的那一版（快照的 `project_files_update_ref`）；
    /// `None` = 没变。兜底路径现场找的就是最新的，恒为 `None`。
    pub project_update: Option<Vec<ProjectFile>>,
    /// 渲染好的 skills 目录块。冻结路径按引用读回受理时那一份；兜底路径现场从注册表渲染。
    /// 子代理不看目录，由调用方丢掉（§12）。
    pub skills: Option<String>,
    /// 记忆作用域（§9.2）。
    pub memory_scope: Option<MemoryScope>,
    /// 记忆模式（冻结的那一份）：召回放行哪些作用域由它与 `agent_id` 定。
    pub memory_mode: MemoryMode,
    /// 冻结的投影设置；`None` = 没有快照或快照早于这一格，调用方按当前配置。
    pub projection: Option<ProjectionContext>,
    /// TUI `!` 直接执行的命令（快照里冻结的那一份）；有值时这一段不经模型。
    pub direct_command: Option<String>,
    /// `task` 给这条 Run 的结果契约（快照里冻结的那一份，§4）。
    pub contract: Option<DelegateContract>,
}

/// 没有冻结快照时兜底身份要用到的那几样：当前配置、会话 workdir、`workspaces/`（§7）。
pub(crate) struct Fallback<'a> {
    pub config: Option<&'a Arc<ConfigHolder>>,
    /// skills 注册表；`None` = 没接（精简装配、关掉 skills 的部署）。
    pub skills: Option<&'a RwLock<SkillRegistry>>,
    pub record: Option<&'a komo_store::repos::session::SessionRecord>,
    pub workspaces: &'a Path,
}

/// 这一段的**身份与能力**（§4.3）。
///
/// 三条路，按优先级：
///
/// 1. **自己那条 `run.accepted` 里冻结的那一份**——正常的交互 Run 都走这条。介入可能
///    一小时之后才答复，那时 Profile 与磁盘都可能已经改过，恢复出来的 Run 不能因此
///    换一副面孔；
/// 2. **父 Run 的快照**——子 Run 受理时拿不到 Profile、工作目录与指令正文（执行器手里
///    只有父的 `CallEnv`），而它本来就是父那次委派的延续：同一个 Agent、同一个目录、
///    同一份指令，只是不能再委派（§4 的深度只有一层，摘名字在 `segment()` 里做）；
/// 3. **当前配置的默认 Agent**——旧行（这次改造之前受理的）与 Cron（没有归属的入口）。
///    §八「已有 Session 归入默认 Agent；旧日志不重写」。
///
/// **没有第四条路**：当前 Profile 不许覆盖一条已经受理的 Run。
pub(crate) async fn identity_for(
    events: &[Event],
    run: &RunId,
    payloads: &PayloadStore,
    fallback: Fallback<'_>,
    catalog: &[ToolDefinition],
) -> Result<Identity, LedgerError> {
    let frozen = frozen_snapshot(events, run).or_else(|| {
        delegate_parent(events, run).and_then(|parent| frozen_snapshot(events, &parent))
    });
    let Some(frozen) = frozen else {
        return Ok(ambient_identity(fallback, catalog));
    };
    // 身份指令按引用读回来（§4.3：存的是内容，不是文件路径）。读不出来 = 会话缺内容，
    // 由调用方停下来报告——拿当前配置编一份提示继续跑，正是"换了一副面孔"。
    let instructions = match &frozen.instructions_ref {
        Some(reference) => {
            let bytes = payloads.open(reference).await.map_err(store_to_ledger)?;
            Some(String::from_utf8(bytes).map_err(|error| {
                LedgerError::Corrupt(format!("冻结的身份指令不是 UTF-8：{error}"))
            })?)
        }
        None => None,
    };
    // 项目规范同理：按引用读回受理时那一版，读不出来照样停下来，不现场再找。
    let project_files = match &frozen.project_files_ref {
        Some(reference) => super::project_files::thaw(payloads, reference).await?,
        None => Vec::new(),
    };
    let project_update = match &frozen.project_files_update_ref {
        Some(reference) => Some(super::project_files::thaw(payloads, reference).await?),
        None => None,
    };
    // skills 目录也一样：按引用读回受理时那段正文，不读活注册表（续跑的系统提示要与受理时
    // 逐字相同）。
    let skills = match &frozen.skills_ref {
        Some(reference) => {
            let bytes = payloads.open(reference).await.map_err(store_to_ledger)?;
            Some(String::from_utf8(bytes).map_err(|error| {
                LedgerError::Corrupt(format!("冻结的 skills 目录不是 UTF-8：{error}"))
            })?)
        }
        None => None,
    };
    Ok(Identity {
        agent_id: frozen.agent_id,
        model: Some(frozen.model),
        surface: frozen.surface,
        // 快照里存的是受理那一刻解析过的真实路径；再解一次是幂等的（目录后来被删掉
        // 也只会把还不存在的那几段按字面接回去）。
        workspace: paths::real_root(&frozen.workspace),
        instructions,
        project_files,
        project_update,
        skills,
        memory_scope: frozen.memory_scope,
        memory_mode: frozen.memory_mode,
        projection: frozen.projection,
        direct_command: frozen.direct_command,
        contract: frozen.contract,
    })
}

/// 没有冻结快照时的兜底身份：**当前**配置里、这个会话归属的那个 Agent。
///
/// 会话没有归属（空串，升级前建的行）或者归属的 Agent 已经从 `[agents]` 里删掉了，才退到
/// 默认 Agent——与 `GatewayState::owner_or_default` / `freeze_run` 同一条规则
/// （`AgentConfig::profile_of`），不是一条各算各的兜底：答错了 Agent 就是拿错误的
/// 身份跑完整个 Run。
///
/// 工作目录的取舍与受理那一步是**同一条规则**（会话 `workdir` → Profile `workspace`
/// → `workspaces/`）：同一件事在两条路上有两个答案，正是 §八 修掉的那类缺口。
fn ambient_identity(fallback: Fallback<'_>, catalog: &[ToolDefinition]) -> Identity {
    let names: Vec<String> = catalog.iter().map(|tool| tool.name.clone()).collect();
    let config = fallback.config.map(|config| config.current());
    let agent_id = fallback
        .record
        .map(|record| record.agent_id.as_str())
        .unwrap_or_default();
    let profile = config
        .as_ref()
        .map(|config| config.agent.profile_of(agent_id));
    let surface = match (profile, fallback.config) {
        (Some(profile), Some(holder)) => {
            komo_agent::surface_of(profile, &names, &holder.home().join("config.toml"))
        }
        // 精简装配（没有配置快照）：这次能用的就是目录里装着的那些。
        _ => AgentSurface::new(names),
    };
    let workspace = fallback
        .record
        .and_then(|record| record.workdir.clone())
        .map(PathBuf::from)
        .or_else(|| profile.and_then(|profile| profile.workspace.clone()))
        .unwrap_or_else(|| fallback.workspaces.to_path_buf());
    let workspace = paths::real_root(&workspace);
    // 没有快照就没有冻结的目录：与指令、项目规范同一个口径，按现在的注册表渲染。
    let skills = fallback.skills.and_then(|registry| {
        super::state::skills_block(&registry.read().expect("skills 注册表"), surface.names())
    });
    Identity {
        agent_id: profile
            .map(|profile| profile.id.clone())
            .unwrap_or_default(),
        // 模型这一格留空：调用方回落到 `runs.model_snapshot`（受理时写下的那一份）。
        model: None,
        surface,
        // 与指令读当前配置同一个口径：没有快照就按现在的磁盘。
        project_files: if profile.is_none_or(|profile| profile.project_files) {
            super::project_files::discover(&workspace)
        } else {
            Vec::new()
        },
        project_update: None,
        skills,
        workspace,
        instructions: profile.and_then(|profile| profile.instructions.clone()),
        memory_scope: profile.and_then(|profile| profile.memory_scope.clone()),
        memory_mode: profile.map(|profile| profile.memory).unwrap_or_default(),
        projection: None,
        direct_command: None,
        contract: None,
    }
}

/// 这一条 Run 受理时冻结下来的身份与能力（§4.3）。
///
/// 它落在**事件**里（`run.accepted`），所以重启之后读得到——这正是"恢复时按它装配"
/// 那句话的落点。
pub(crate) fn frozen_snapshot(events: &[Event], run: &RunId) -> Option<Box<RunSnapshot>> {
    events.iter().find_map(|event| match &event.payload {
        EventPayload::RunAccepted(body) if event.run.as_ref() == Some(run) => body.snapshot.clone(),
        _ => None,
    })
}

/// fork 任务会话（`docs/background-tasks.md` §2b）的来处与冻结前缀：回放窗口里第一条
/// 快照带 `fork` 的 Run 的那一份。`None` = 不是 fork，或者 fork 那条 Run 已在边界之前。
///
/// 前缀读不出来 = 会话缺内容（§8.3）：由调用方停下来报告，不能当成"没有前缀"把一个只有
/// 任务正文的请求发出去——那会让 fork 答一个它看不见上下文的问题。
pub(crate) async fn fork_prefix(
    events: &[Event],
    surface: &Surface,
    payloads: &PayloadStore,
) -> Result<Option<(ForkOrigin, ForkPrefix)>, LedgerError> {
    let in_window = |run: &RunId| {
        surface
            .replay()
            .iter()
            .any(|message| message.run.as_ref() == Some(run))
    };
    let origin = events
        .iter()
        .find_map(|event| match (&event.payload, &event.run) {
            (EventPayload::RunAccepted(body), Some(run)) if in_window(run) => body
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.fork.clone()),
            _ => None,
        });
    let Some(origin) = origin else {
        return Ok(None);
    };
    let prefix: ForkPrefix = payloads
        .open_json(&origin.prefix)
        .await
        .map_err(store_to_ledger)?;
    Ok(Some((origin, prefix)))
}

/// 派这一条 Run 的那条 Run（§4；普通 Run 是 `None`）。
fn delegate_parent(events: &[Event], run: &RunId) -> Option<RunId> {
    events.iter().find_map(|event| match &event.payload {
        EventPayload::RunAccepted(body) if event.run.as_ref() == Some(run) => {
            body.delegate.as_ref().map(|spec| spec.parent.clone())
        }
        _ => None,
    })
}

/// 这一次召回放行哪些作用域（§9.2）；`None` = 这一段不召回。
///
/// `memory = "off"` 不召回；`"private"` 只放行这个 Agent 自己的 `agent:<id>`——**不带
/// Personal**，那是操作者的个人记忆，私有记忆的 Profile 不该碰；`"shared"` 看 Profile
/// 的 `memory_scope`：没写 = **不按作用域过滤**（`RecallQuery::scopes` 空就是"全部"），
/// 写了就只召回那一个，外加 [`MemoryScope::Personal`]——用户资料是**显式共享**的那一份，
/// 每个 Agent 都该看得见。
pub(crate) fn recall_scopes(
    mode: MemoryMode,
    agent_id: &str,
    scope: Option<&MemoryScope>,
) -> Option<Vec<MemoryScope>> {
    match mode {
        MemoryMode::Off => None,
        MemoryMode::Private => Some(vec![MemoryScope::Agent {
            agent_id: agent_id.to_string(),
        }]),
        MemoryMode::Shared => {
            let Some(scope) = scope else {
                return Some(Vec::new());
            };
            let mut scopes = vec![scope.clone()];
            if *scope != MemoryScope::Personal {
                scopes.push(MemoryScope::Personal);
            }
            Some(scopes)
        }
    }
}

/// 这一段注入哪些记忆（§9.4、§9.7）。
///
/// 查询文本是回放面上**最后一条用户消息**——「当前用户输入 + 少量任务上下文」
/// （§9.4）。续跑时先把检查点里记的那一批交给 [`MemoryManager::prepare`] 重新核对：
/// 活下来的沿用（不必再问一次 embedding），已经遗忘或改了版本的掉出去（§9.7）。
///
/// `scopes` 来自 Profile（`memory_scope`）：**过滤发生在检索里**，不是拿到结果之后再
/// 遮掉别人 Agent 的记录（§五）。
///
/// 这一段在日志里记下过注入段（`memory.injected`）就交给 `prepare_segment` 当后备：进程
/// 里的锚重启就没了，按它逐字复用才不会换掉整段系统提示（§9.4）。这一次用的与记下的不同
/// （第一次钉、换段、或有条目被遗忘），就记下新的一块；记不下来只留一条 warn——代价是
/// 重启之后多召回一次，不该让这一轮失败。
pub(crate) async fn recall_for(
    memories: Option<&Arc<MemoryManager>>,
    checkpoints: Option<&CheckpointStore>,
    ledger: &dyn Ledger,
    payloads: &PayloadStore,
    session: &komo_kernel::types::ids::SessionId,
    run: &RunId,
    surface: &Surface,
    scopes: &[MemoryScope],
    session_kind: &'static str,
) -> Injection {
    let Some(memories) = memories else {
        return Injection::default();
    };
    let carried = match checkpoints {
        Some(store) => match store.latest(session).await {
            Ok(Some(record)) => record.memories,
            Ok(None) => Vec::new(),
            Err(error) => {
                tracing::debug!(%error, "读不出检查点，这一段重新召回");
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    let text = latest_user_text(surface).unwrap_or_default();
    let recorded = recorded_injection(payloads, surface).await;
    // **按"这一段对话"取**，不是按这一轮重新召回（§9.4）：注入段在 system 消息里，
    // 每轮重算一次就等于每轮把服务端前缀缓存打掉一次。
    let injection = memories
        .prepare_segment(
            session,
            run,
            &text,
            &carried,
            surface.boundary(),
            scopes,
            recorded.clone(),
        )
        .await;
    if memories.enabled() && recorded.as_ref() != Some(&injection) {
        let event = MemoryInjected {
            text: injection.text.clone(),
            text_ref: None,
            uses: injection.uses.clone(),
            recall: injection.diagnostics.clone(),
        };
        if let Err(error) = ledger.record_injection(run, event).await {
            tracing::warn!(%error, %run, "这一段的记忆注入没记进日志，重启之后会重新召回");
        }
        tracing::event!(
            target: "komo::metrics",
            tracing::Level::DEBUG,
            {
            monotonic_counter.komo.memory.injected = injection.uses.len() as u64,
            komo.session.kind = session_kind,
            }
        );
    }
    injection
}

/// 这一段日志里记下的注入段；外置的正文按引用读回。读不回来就当没记过（重新召回只是贵，
/// 不是错），不让整段装配失败。
async fn recorded_injection(payloads: &PayloadStore, surface: &Surface) -> Option<Injection> {
    let recorded = surface.injection()?;
    let text = match (&recorded.text, &recorded.text_ref) {
        (Some(text), _) => Some(text.clone()),
        (None, Some(reference)) => match payloads.open(reference).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => Some(text),
                Err(error) => {
                    tracing::warn!(%error, "记下的注入段不是 UTF-8，这一段重新召回");
                    return None;
                }
            },
            Err(error) => {
                tracing::warn!(%error, "记下的注入段读不回来，这一段重新召回");
                return None;
            }
        },
        (None, None) => None,
    };
    Some(Injection {
        text,
        uses: recorded.uses.clone(),
        diagnostics: None,
    })
}

/// 一条消息的正文：内联的那份优先，没有就按引用读回来（§8.3）。
///
/// 只读内联那份会让超限的正文在回放里变成**空消息**：用户输入过 4 KiB 时，模型看到的是
/// 一个空的 user，而它该看到的是原话。外置正文的哈希由 `PayloadStore::open` 校验，对不上
/// 就是会话缺内容，不返回内容。
pub(crate) async fn message_text(
    payloads: &PayloadStore,
    message: &SurfaceMessage,
) -> Result<Option<String>, LedgerError> {
    if let Some(text) = &message.text {
        return Ok(Some(text.clone()));
    }
    let Some(reference) = &message.text_ref else {
        return Ok(None);
    };
    let bytes = payloads.open(reference).await.map_err(store_to_ledger)?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| LedgerError::Corrupt(format!("外置正文不是 UTF-8：{error}")))
}

/// history 的 I/O 那一半：只对 `entries` 选中的条目读正文与 `output.json`（§7、§8）。
///
/// 压缩摘要（`EntryKind::Summary`）也是一条这样的条目：外置的 `summary_ref` 就在它的
/// `text_ref` 上，按同一条路读回来。
///
/// **只有 `Protocol` 条目才会去读工具输出**：`Transcript` 条目不带工具往返，`outputs`
/// 恒为空。读不回来的正文（外置引用坏了）让整段装配失败——调用方按 §8.4 停下来报告，
/// 不把一条空消息当成"用户就是这么说的"发出去；`output.json` 读不回来则不是错误，只是
/// 退回账本里那份预览，交给 [`komo_agent::context::history::to_replay_messages`] 处理。
pub(crate) async fn resolve_history<'s>(
    entries: Vec<Entry<'s>>,
    payloads: &PayloadStore,
    outputs: Option<&dyn ToolOutputStore>,
) -> Result<Vec<ResolvedMessage<'s>>, LedgerError> {
    let mut resolved = Vec::with_capacity(entries.len());
    for entry in entries {
        let text = message_text(payloads, entry.message).await?;
        let mut stored = Vec::new();
        if matches!(entry.kind, EntryKind::Protocol) {
            for result in &entry.message.tool_results {
                let verified = match outputs {
                    Some(outputs) => outputs.open(&result.output).await.ok(),
                    None => None,
                };
                let Some(verified) = verified else {
                    stored.push(None);
                    continue;
                };
                // 图片字节按引用读回来、校验过才附上；读不回来的那张只少这一张，正文照旧。
                let mut image_bytes = Vec::new();
                if let Some(outputs) = outputs {
                    for image in &verified.body.images {
                        match outputs.open_image(image).await {
                            Ok(bytes) => image_bytes.push(ToolImage {
                                mime: image.mime.clone(),
                                bytes,
                            }),
                            Err(error) => tracing::warn!(
                                path = %image.content.path,
                                %error,
                                "工具结果里的图片读不回来，这次不附"
                            ),
                        }
                    }
                }
                stored.push(Some(StoredOutput {
                    preview: verified.body.preview.clone(),
                    artifacts: verified.body.artifacts.clone(),
                    images: verified.body.images.clone(),
                    image_bytes,
                }));
            }
        }
        resolved.push(ResolvedMessage {
            entry,
            text,
            outputs: stored,
        });
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    //! 真的要读 `PayloadStore` 的那几条测试：`entries` 的选窗口本身是纯的（测试在
    //! `komo-agent`），这里测的是 `resolve_history` 的 I/O 那一半——外置正文真的读得回来、
    //! 读不回来就不装作读到了。走的是生产路径：`entries` → `resolve_history` →
    //! `komo_agent::context::assemble`。

    use super::*;

    fn agent(id: &str) -> MemoryScope {
        MemoryScope::Agent {
            agent_id: id.into(),
        }
    }

    #[test]
    fn recall_scopes_follow_the_memory_mode() {
        assert_eq!(recall_scopes(MemoryMode::Off, "helper", None), None);
        // 私有：只有它自己的作用域，Profile 的 `memory_scope` 不参与、Personal 不带。
        assert_eq!(
            recall_scopes(MemoryMode::Private, "helper", Some(&MemoryScope::Personal)),
            Some(vec![agent("helper")])
        );
        // 共享：与改造之前一样。
        assert_eq!(
            recall_scopes(MemoryMode::Shared, "assistant", None),
            Some(vec![])
        );
        let project = MemoryScope::Project {
            project_id: "komo".into(),
        };
        assert_eq!(
            recall_scopes(MemoryMode::Shared, "assistant", Some(&project)),
            Some(vec![project, MemoryScope::Personal])
        );
    }

    use komo_agent::context::history::{ReplayScope, entries};
    use komo_agent::context::{ContextInput, InvocationContext, assemble};
    use komo_kernel::events::{Event, EventPayload, RunAccepted, RunStarted};
    use komo_kernel::fold::fold;
    use komo_kernel::types::digest::ContentHash;
    use komo_kernel::types::ids::{EventId, ExecutorId, RequestKey, RunId, Seq, SessionId};
    use komo_kernel::types::plan::PlanSource;
    use komo_kernel::types::refs::PayloadRef;

    fn payloads() -> PayloadStore {
        let dir = tempfile::tempdir().expect("临时目录");
        PayloadStore::new(komo_store::SessionPaths::at(dir.keep()))
    }

    fn event(seq: u64, run: &RunId, payload: EventPayload) -> Event {
        Event {
            entry: None,
            v: 1,
            seq: Seq(seq),
            event_id: EventId::from_raw(format!("evt-{seq}")),
            session: SessionId::from_raw("sess-1"),
            run: Some(run.clone()),
            ts: time::macros::datetime!(2026-09-16 08:00:00 UTC),
            payload,
        }
    }

    fn accepted_as(
        run: &RunId,
        seq: u64,
        text: Option<&str>,
        text_ref: Option<PayloadRef>,
    ) -> Event {
        event(
            seq,
            run,
            EventPayload::RunAccepted(RunAccepted {
                request_key: RequestKey::new(format!("key-{seq}")),
                input_hash: ContentHash::of_str(text.unwrap_or_default()),
                text: text.map(str::to_string),
                text_ref,
                source: PlanSource::Interactive {
                    session: SessionId::from_raw("sess-1"),
                },
                peer: None,
                model: None,
                effort: None,
                delegate: None,
                snapshot: None,
                interject: false,
            }),
        )
    }

    fn started(run: &RunId, seq: u64) -> Event {
        event(
            seq,
            run,
            EventPayload::RunStarted(RunStarted {
                executor: ExecutorId::from_raw("exec-1"),
                generation: 1,
            }),
        )
    }

    async fn context_of(
        events: &[Event],
        run: &RunId,
        payloads: &PayloadStore,
    ) -> Result<komo_agent::context::AgentContext, LedgerError> {
        let surface = fold(events);
        let selected = entries(&surface, ReplayScope::Conversation(run));
        let resolved = resolve_history(selected, payloads, None).await?;
        Ok(assemble(ContextInput {
            instructions: None,
            project_files: Vec::new(),
            project_update: None,
            contract: None,
            workspace: PathBuf::from("/work"),
            tools: Vec::new(),
            mcp_servers: Vec::new(),
            history: resolved,
            decayed: Default::default(),
            memory: None,
            skills: None,
            invocation: InvocationContext::Main,
            projection: ProjectionContext {
                model_result_bytes: 8 * 1024,
                decay: None,
            },
            fork: None,
        }))
    }

    /// 超限的正文外置在 `payloads/` 里：回放要按引用读回来（§8.3）。
    ///
    /// 只读内联那份的话，用户输入过 4 KiB 时模型看到的是一个**空的 user 消息**。
    #[tokio::test]
    async fn an_externalized_message_comes_back_by_reference() {
        let payloads = payloads();
        let big = "把这段话原样带回来。".repeat(600);
        assert!(big.len() > 4 * 1024, "要真的过内联上限");
        let question = payloads.put(big.as_bytes()).await.expect("外置得进去");
        let reply = payloads
            .put(big.as_bytes())
            .await
            .expect("同一段正文复用一份");

        let first = RunId::from_raw("run-1");
        let second = RunId::from_raw("run-2");
        let events = vec![
            accepted_as(&first, 1, None, Some(question)),
            started(&first, 2),
            // 一条没有正文的轮次：它不该抢走"最后答了什么"的位置。
            event(
                3,
                &first,
                EventPayload::MessageAssistant(komo_kernel::events::MessageAssistant {
                    round: 1,
                    text: None,
                    text_ref: None,
                    tool_calls: vec![],
                    provider_blocks: None,
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    reasoning_tokens: None,
                }),
            ),
            event(
                4,
                &first,
                EventPayload::MessageAssistant(komo_kernel::events::MessageAssistant {
                    round: 2,
                    text: None,
                    text_ref: Some(reply),
                    tool_calls: vec![],
                    provider_blocks: None,
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    reasoning_tokens: None,
                }),
            ),
            accepted_as(&second, 5, Some("接着上面那句"), None),
            started(&second, 6),
        ];

        let context = context_of(&events, &second, &payloads)
            .await
            .expect("读得出来");
        assert_eq!(
            context.messages[0].text.as_deref(),
            Some(big.as_str()),
            "外置的用户输入要按引用读回来"
        );
        assert_eq!(
            context.messages[1].text.as_deref(),
            Some(big.as_str()),
            "外置的模型回复同理（最后一条带正文的那条）"
        );
    }

    /// 引用读不出来 = 会话缺内容：停下来报告，而不是把一条空消息发出去（§8.3）。
    #[tokio::test]
    async fn a_payload_that_cannot_be_read_stops_the_segment() {
        let missing = payloads()
            .put("这份正文不在那个目录里".as_bytes())
            .await
            .expect("外置得进去");
        let run = RunId::from_raw("run-1");
        let events = vec![accepted_as(&run, 1, None, Some(missing)), started(&run, 2)];

        // 故意用一个**不同的** `PayloadStore`：那份引用不在这里，读不出来。
        let error = context_of(&events, &run, &payloads())
            .await
            .expect_err("读不出来就不该装作读到了");
        assert!(matches!(error, LedgerError::Corrupt(_)), "{error:?}");
    }

    /// 没有冻结快照时，兜底身份要看会话自己的 `agent_id`——配的是哪个 Profile 就用
    /// 哪个的指令与能力面，不能总是退到默认 Agent。
    #[tokio::test]
    async fn ambient_identity_uses_the_sessions_agent_when_it_names_a_configured_profile() {
        use std::collections::BTreeMap;

        use komo_kernel::types::agent::{AgentConfig, AgentProfile};
        use komo_kernel::types::status::SessionState;
        use komo_runtime::config::{EffortCapabilities, Loaded, Secrets, Sources};
        use komo_store::models::SessionKind;
        use komo_store::repos::session::SessionRecord;

        let payloads = payloads();
        let run = RunId::from_raw("run-1");
        // `accepted_as` 定死不带快照：旧行 / 没有冻结身份的子 Run 走的都是这条路。
        let events = vec![
            accepted_as(&run, 1, Some("查一下 3 号房的空调"), None),
            started(&run, 2),
        ];

        let mut snapshot = komo_kernel::test_support::snapshot_fixture();
        snapshot.agent = AgentConfig {
            default_agent: "assistant".into(),
            agents: BTreeMap::from([
                ("assistant".into(), AgentProfile::new("assistant")),
                (
                    "coder".into(),
                    AgentProfile {
                        instructions: Some("你是 coder，只管写代码。".into()),
                        tools: Some(vec!["read".into()]),
                        ..AgentProfile::new("coder")
                    },
                ),
            ]),
        };
        let home = PathBuf::from("/tmp/komo-ambient-identity-test");
        let holder = Arc::new(ConfigHolder::adopt(
            Loaded {
                snapshot: Arc::new(snapshot),
                secrets: Arc::new(Secrets::new()),
                issues: Vec::new(),
                home: home.clone(),
                sources: Sources::under(&home),
            },
            EffortCapabilities::builtin(),
        ));

        let session_record = SessionRecord {
            session: SessionId::from_raw("sess-1"),
            title: String::new(),
            origin: "chat:telegram:1".into(),
            agent_id: "coder".into(),
            kind: SessionKind::Main,
            workdir: None,
            current_run: None,
            jsonl_path: String::new(),
            applied_seq: Seq::ZERO,
            applied_bytes: 0,
            state: SessionState::Active,
            state_changed_at: time::macros::datetime!(2026-09-24 08:00:00 UTC),
        };

        let catalog = vec![
            ToolDefinition {
                name: "read".into(),
                description: String::new(),
                parameters: serde_json::Value::Null,
                doc: None,
            },
            ToolDefinition {
                name: "bash".into(),
                description: String::new(),
                parameters: serde_json::Value::Null,
                doc: None,
            },
        ];
        let workspaces = PathBuf::from("/workspaces");

        let identity = identity_for(
            &events,
            &run,
            &payloads,
            Fallback {
                config: Some(&holder),
                skills: None,
                record: Some(&session_record),
                workspaces: &workspaces,
            },
            &catalog,
        )
        .await
        .expect("没有冻结快照时兜底也读得出来");

        assert_eq!(
            identity.agent_id, "coder",
            "兜底身份要按会话的归属挑 Profile"
        );
        assert_eq!(
            identity.instructions.as_deref(),
            Some("你是 coder，只管写代码。")
        );
        assert_eq!(
            identity.surface.names(),
            ["read"],
            "coder 的能力面只留它自己写的那个工具"
        );
    }
}
