//! 领到一个 Run 之后，这一段要的上下文从哪来（`SegmentSource` 的生产实现）。
//!
//! 三样东西在这里凑齐（`agent::handler` 的模块注释列的就是它们）：
//!
//! - **工作目录与已授权根**：Session 的 `workdir`，没有就是 `workspaces/`。
//! - **`TurnRequest`**：系统提示 + 回放窗口（最新一个 `conversation.boundary` 之后、
//!   属于这一段对话的消息——正在跑的那条 Run 带完整协议，历史 Run 只发布正文，见
//!   [`ReplayScope`]）+ 执行器挂着的工具 Schema。
//! - **恢复位置**：这个 Run 还有没有没收尾的调用。有就把它们原样交回执行器——**沿用
//!   同一份计划**，因为审批绑定的是计划的哈希，重新 prepare 会换一个哈希（§7.4）。
//!
//! **记忆在这里召回，不在提示里拼**（§9.4）：这一段装配时按最新一句用户输入召回一次，
//! 正文交给 `LlmFactory::with_preamble` 挂到系统提示后面，而用到的条目连同它们的
//! revision 写进 [`TurnRequest::memories`]——那是审计证据，resume 时要按它重新核对
//! （§9.7）。续跑时先拿检查点里记的那一批去核对，**过期或已遗忘的复活不了**。
//
// TODO(decide: 系统提示的正文文档没有规定（§5.6 只说 skills 目录行是启动快照）。W4 给
// 的那段最小提示留到 SkillRegistry 接进来时再换。)

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_agent::DELEGATE_TOOL;
use komo_kernel::events::{Event, EventPayload};
use komo_kernel::fold::{Surface, SurfaceMessage, fold};
use komo_kernel::projection::{ProjectionContext, ToolResultFacts, project};
use komo_kernel::traits::{ApprovalRepo, Ledger, LedgerError, ToolOutputStore};
use komo_kernel::types::agent::RunSnapshot;
use komo_kernel::types::delegate::DelegateSpec;
use komo_kernel::types::ids::{RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::memory::MemoryScope;
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::plan::ExecutionPlan;
use komo_kernel::types::resource::{ResourceMounts, SkillMount};
use komo_kernel::types::status::ToolCallState;
use komo_kernel::types::surface::AgentSurface;
use komo_kernel::types::tool::{CancelToken, ToolDefinition, WorkspaceRoot};
use komo_kernel::types::turn::{
    ReplayMessage, Role, ToolCallRequest, ToolResultForModel, TurnRequest,
};
use komo_runtime::agent::handler::SegmentSource;
use komo_runtime::agent::{Budget, ResumedRound, RetryBudget, Segment};
use komo_runtime::config::ConfigHolder;
use komo_runtime::executor::{CallEnv, CallRequest, ToolExecutor, resumed_from};
use komo_runtime::memory::MemoryManager;
use komo_runtime::scheduler::HandlerError;
use komo_runtime::tools::paths;
use komo_store::db::store_to_ledger;
use komo_store::{CheckpointStore, Db, PayloadStore, RecoveryStore};

use super::ledgers::RoutedLedger;

/// 装配执行段，并持有每个 Run 的取消开关。
pub struct GatewaySegments {
    routed: Arc<RoutedLedger>,
    db: Db,
    approvals: Arc<dyn ApprovalRepo>,
    /// 读不出来的会话要停在 `needs_attention` 上，而不是被反复领取——写那一笔要它。
    recovery: RecoveryStore,
    workspaces: PathBuf,
    /// `sessions/` 那一层目录。**根拿它算**：`<sessions>/<session>/tool-output` 是这个
    /// Session 自己的输出（§8.3 要求它可被 `read` 只读访问）。`None` = 精简装配。
    session_files: Option<PathBuf>,
    /// 回放时要读回 `output.json` 里那份完整正文（投影用），所以这里得有输出存储。
    outputs: Option<Arc<dyn ToolOutputStore>>,
    /// 交给模型的正文预算**每次装配时从当前配置快照读一次**（§3：读者按次读当前快照）。
    /// 拿它填进 `CallEnv`，回放那一侧用同一个值——两处各写一个数就会让"刚跑完"和"回放"
    /// 渲染出不一样的正文。
    config: Option<Arc<ConfigHolder>>,
    max_rounds: u32,
    max_retries: u32,
    cancels: Mutex<BTreeMap<RunId, CancelToken>>,
    /// `None` = 这台 Gateway 没有记忆这一层（测试里的精简装配）。
    memories: Option<Arc<MemoryManager>>,
    checkpoints: Option<CheckpointStore>,
    /// Cron Job 的**执行预算**要从 Job 上读（§10：每个 Job 有自己的执行预算）。
    /// `None` = 没接 Cron 这一层，所有 Run 都用全局的 `max_rounds`。
    cron: Option<Arc<dyn komo_kernel::traits::CronRepo>>,
    /// §5.6 那份**活的** skill 注册表。与 Gateway 共享同一个 `Arc`。
    ///
    /// 提示里的那一块目录行**每次装配现渲染**（[`Self::skills_block`]），`skill://` 的挂载点
    /// 也从这里抄——两处同一个来源，"提示里看见的名字读不到"就不是一种可能了。
    skills: Option<Arc<std::sync::RwLock<komo_agent::skills::SkillRegistry>>>,
    /// 执行器。**只用来渲染交给模型的工具 Schema**（[`ToolExecutor::definitions_for`]）：
    /// 能力面已经由冻结快照（或它的兜底）给出，"这次能用哪些工具"的判据只有那一处，
    /// 这里不该再有一份。`None` = 精简装配（没有执行器的那几个单元测试）。
    executor: Option<Arc<ToolExecutor>>,
}

impl std::fmt::Debug for GatewaySegments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewaySegments").finish_non_exhaustive()
    }
}

/// 一段执行要用到的**身份与能力**（§4.3）。
///
/// 它要么来自 `run.accepted` 里冻结的那一份，要么（旧行 / Cron / 没有快照的子 Run）由
/// 当前配置的默认 Agent 兜底算出来——见 [`GatewaySegments::identity_for`]。这里的每一格
/// 都进这一段的装配：提示、Schema、工作目录、记忆作用域。
struct Identity {
    /// 这一段的 Agent（进日志；判据是下面那几样）。
    agent_id: String,
    /// 冻结下来的模型；`None` = 兜底路径，用 `runs.model_snapshot` 那一份。
    model: Option<ModelConfig>,
    /// 这次允许调用的工具。**Schema 与执行器查找用的是同一个它**（§4 末）。
    surface: AgentSurface,
    /// 解析过的真实工作目录。
    workspace: PathBuf,
    /// 身份指令正文（按引用读回来的那一份）。
    instructions: Option<String>,
    /// 记忆作用域（§9.2）。
    memory_scope: Option<MemoryScope>,
}

impl GatewaySegments {
    pub fn new(
        routed: Arc<RoutedLedger>,
        db: Db,
        approvals: Arc<dyn ApprovalRepo>,
        recovery: RecoveryStore,
        workspaces: PathBuf,
        max_rounds: u32,
        max_retries: u32,
    ) -> Self {
        GatewaySegments {
            routed,
            db,
            approvals,
            recovery,
            workspaces,
            session_files: None,
            outputs: None,
            config: None,
            max_rounds,
            max_retries,
            cancels: Mutex::new(BTreeMap::new()),
            cron: None,
            memories: None,
            checkpoints: None,
            skills: None,
            executor: None,
        }
    }

    /// 接上执行器：交回模型的工具 Schema 由它按能力面渲染（[`ToolExecutor::definitions_for`]）。
    pub fn with_executor(mut self, executor: Arc<ToolExecutor>) -> Self {
        self.executor = Some(executor);
        self
    }

    /// 接上 §5.6 那份活的 skill 注册表。
    pub fn with_skills(
        mut self,
        skills: Arc<std::sync::RwLock<komo_agent::skills::SkillRegistry>>,
    ) -> Self {
        self.skills = Some(skills);
        self
    }

    /// 接上 `sessions/` 那一层：接上之后，**这个 Session 自己的输出与产物**成为一段可以
    /// 用的只读根（§8.3）。
    pub fn with_session_files(mut self, sessions_dir: PathBuf) -> Self {
        self.session_files = Some(sessions_dir);
        self
    }

    /// 接上投影要用的输出存储（§8.3）。
    pub fn with_projection(mut self, outputs: Arc<dyn ToolOutputStore>) -> Self {
        self.outputs = Some(outputs);
        self
    }

    /// 接上配置：输出预算按当前快照热生效（§3）。
    pub fn with_config(mut self, config: Arc<ConfigHolder>) -> Self {
        self.config = Some(config);
        self
    }

    /// 这一次装配用多少正文预算。
    fn model_result_bytes(&self) -> usize {
        self.config
            .as_ref()
            .map(|config| config.current().execution.model_result_bytes)
            .unwrap_or(komo_kernel::projection::DEFAULT_MODEL_RESULT_BYTES)
    }

    /// 这一段能用哪些根。见 [`run_roots`]。
    fn roots_for(&self, session: &SessionId, cwd: PathBuf) -> Vec<WorkspaceRoot> {
        run_roots(
            self.session_files.as_deref(),
            session,
            cwd,
            &self.skill_dirs(),
        )
    }

    /// 这次装配要认的那些 skill 根（顺序即注册表的优先级）。
    fn skill_dirs(&self) -> Vec<PathBuf> {
        self.skills
            .as_ref()
            .map(|skills| skills.read().expect("skills 注册表").dirs().to_vec())
            .unwrap_or_default()
    }

    /// 现在这一块 skills 目录行：按注册表现渲染，工具门控按**这一次**拿到的这套工具。
    /// 没接注册表就是空的——精简装配（测试、`skills` 关掉的部署）不该因为少这一块就少别的。
    fn skills_block(&self, tools: &[ToolDefinition]) -> String {
        let Some(skills) = &self.skills else {
            return String::new();
        };
        let names: Vec<String> = tools.iter().map(|tool| tool.name.clone()).collect();
        crate::service::state::skills_block(&skills.read().expect("skills 注册表"), &names)
    }

    /// 接上记忆这一层。
    pub fn with_memories(
        mut self,
        memories: Arc<MemoryManager>,
        checkpoints: CheckpointStore,
    ) -> Self {
        self.memories = Some(memories);
        self.checkpoints = Some(checkpoints);
        self
    }

    /// 接上 Cron，这样一个 Cron Run 用的是**它那个 Job 的**执行预算（§10）。
    pub fn with_cron(mut self, cron: Arc<dyn komo_kernel::traits::CronRepo>) -> Self {
        self.cron = Some(cron);
        self
    }

    /// 这一段最多跑几轮。
    ///
    /// Cron Job 可以有自己的预算；读不出那个 Job（被删了、读不出来）就回到全局值——
    /// **不要因为读不到预算就不跑**，那会把一次配置问题变成一次静默的失败。
    async fn max_rounds_for(&self, source: &komo_kernel::types::plan::PlanSource) -> u32 {
        let komo_kernel::types::plan::PlanSource::Cron { job, .. } = source else {
            return self.max_rounds;
        };
        let Some(cron) = &self.cron else {
            return self.max_rounds;
        };
        match cron.get(job).await {
            Ok(Some(job)) => job.max_rounds.unwrap_or(self.max_rounds),
            Ok(None) => self.max_rounds,
            Err(error) => {
                tracing::warn!(%error, %job, "读不出这个 Job 的执行预算，用全局的");
                self.max_rounds
            }
        }
    }

    /// 这个 Run 的取消开关。**取消通过明确操作发起**（§13.1）——CLI 退出不取消。
    pub fn cancel(&self, run: &RunId) {
        if let Some(token) = self.cancels.lock().expect("取消表").get(run) {
            token.cancel();
        }
    }

    fn token_for(&self, run: &RunId) -> CancelToken {
        let mut cancels = self.cancels.lock().expect("取消表");
        // 上一段留下的那个可能已经被按过；每一段拿一个新的。
        let token = CancelToken::new();
        cancels.insert(run.clone(), token.clone());
        token
    }

    /// 读完这个 Session 的日志。
    async fn events_of(&self, session: &SessionId) -> Result<Vec<Event>, HandlerError> {
        let mut all = Vec::new();
        let mut from = Seq::ZERO;
        loop {
            let batch = self.routed.read(session, from, 0).await?;
            if batch.events.is_empty() {
                return Ok(all);
            }
            for event in batch.events {
                from = from.max(event.seq);
                all.push(event);
            }
            if batch.next.is_none() {
                return Ok(all);
            }
        }
    }
}

#[async_trait]
impl SegmentSource for GatewaySegments {
    async fn segment(
        &self,
        claimed: &komo_kernel::types::status::Claimed,
        catalog: Vec<ToolDefinition>,
    ) -> Result<Segment, HandlerError> {
        let run = claimed.run.clone();
        let session = self
            .routed
            .session_of_run(&run)
            .await
            .map_err(HandlerError::Ledger)?;
        // 续跑要按调用号找回 Session：把这个会话的调用与尝试先补记进来。
        if let Err(error) = self.routed.learn(&session).await {
            return Err(self.halt_if_corrupt(&run, error).await);
        }

        let record = komo_store::repos::runs::get(&self.db, &run)
            .await?
            .ok_or_else(|| HandlerError::Failed(format!("run {run} 不在账本里")))?;
        let session_record = komo_store::repos::session::get(&self.db, &session).await?;

        let events = match self.events_of(&session).await {
            Ok(events) => events,
            Err(HandlerError::Ledger(error)) => {
                return Err(self.halt_if_corrupt(&run, error).await);
            }
            Err(other) => return Err(other),
        };
        let surface = fold(&events);
        let rounds_so_far = surface
            .runs
            .get(&run)
            .map(|view| view.rounds)
            .unwrap_or_default();

        let model_result_bytes = self.model_result_bytes();
        // 外置正文的来源：窗口里的用户输入与模型回复、以及身份指令正文都可能在这里。
        let payloads = self.payloads_for(&session);

        // **这一段的身份与能力**（§4.3）：用它自己那条 `run.accepted` 里冻结下来的那一份，
        // 而不是当前的 Profile 与工具目录。审批可能一小时之后才答复，那时配置与磁盘都可能
        // 已经改过——恢复出来的 Run 不能因此换一副面孔。
        let mut identity = match self
            .identity_for(&events, &run, &session, session_record.as_ref(), &catalog)
            .await
        {
            Ok(identity) => identity,
            // 身份指令的正文按引用读不出来 = 会话缺内容（§8.3）：停下来报告。用当前配置
            // 编一份提示继续跑，正是"恢复出来的 Run 换了一副面孔"。
            Err(error) => return Err(self.halt_if_corrupt(&run, error).await),
        };

        // 这是一条**子代理**吗？（§4）是的话，它的契约与预算跟着它走——父侧派它时给的那份，
        // 落在它自己的 `run.accepted` 里，所以重启之后也读得到。
        let delegate = surface
            .runs
            .get(&run)
            .and_then(|view| view.delegate.clone());

        // 深度只有一层（§4）：子代理的能力面里**没有 `delegate`**，不管那一份能力面是从
        // 冻结快照继承来的还是兜底算出来的。runtime 的编排里还留着第二道，管的是"有人把
        // 含 `delegate` 的能力面硬塞给子代理"。
        if delegate.is_some() {
            identity.surface = without_delegate(identity.surface);
        }
        tracing::debug!(
            run = %run,
            session = %session,
            agent = %identity.agent_id,
            tools = ?identity.surface.names(),
            workspace = %identity.workspace.display(),
            "这一段按冻结的身份与能力装配"
        );

        // **根必须是真实路径**：工具解析目标时解掉符号链接（`tools::paths::resolve`），
        // 根停在字面上就会让 workspace 里的动作被判成"范围外"（macOS 的 `/tmp`、`/var`
        // 都是链接）。两边同一个口径，前缀匹配才是"在不在这个根里"。
        //
        // 目录取**冻结快照里那一个**：`profile.workspace` 与会话 `workdir` 谁优先是受理
        // 那一刻定下来的（见 `GatewayState::freeze_run`），恢复时照用。
        let cwd = identity.workspace.clone();
        let roots = self.roots_for(&session, cwd.clone());
        // `artifact://` 的两处都落在这个会话自己的内容目录里（`artifact://` 只认它）。
        let session_root = self.session_files.as_ref().map(|dir| {
            paths::real_root(
                komo_store::SessionPaths::new(dir, &session)
                    .root()
                    .to_path_buf()
                    .as_path(),
            )
        });

        // 子代理只拿得到任务本身：不注入记忆、不列 Skills、也**不带上父的对话历史**（它的
        // 回放窗口就是自己那条 Run，见下）。自包含这件事是父侧的责任，提示词里对它也说了。
        let memories = match &delegate {
            Some(_) => Vec::new(),
            // 记忆作用域来自 Profile（§9.2）：只召回这一个作用域，外加显式共享的用户资料。
            None => {
                let scopes = recall_scopes(identity.memory_scope.as_ref());
                self.recall_for(&session, &run, &surface, &scopes).await
            }
        };

        // 回放给模型的是**这一段对话**，不是这一条 Run（§8.3）。
        let scope = match &delegate {
            Some(_) => ReplayScope::Run(&run),
            None => ReplayScope::Conversation(&run),
        };

        // 交给模型的 Schema 是**能力面**渲染出来的（§4 末）：一个来源，两处用——执行器查的
        // 是同一份 `AgentSurface`（`CallEnv::surface`）。两边各拿一份就会出现"schema 里没有
        // 这个名字、执行器却查得到"。
        let tools = match &self.executor {
            Some(executor) => executor.definitions_for(&identity.surface),
            // 精简装配（没有执行器）：目录里这一段允许的那些就是 Schema——判据还是那一份
            // 能力面，不是又一句"这次能用什么"。
            None => catalog
                .into_iter()
                .filter(|tool| identity.surface.allows(&tool.name))
                .collect(),
        };
        let prompt = match &delegate {
            Some(spec) => subagent_prompt(&cwd, &tools, spec),
            None => system_prompt(&cwd, &tools, &self.skills_block(&tools)),
        };
        // 身份指令是系统提示里**最靠前的那一段**（§4.3），基座提示排在它后面。它从冻结
        // 快照指向的正文读回来（不是重新去读配置文件），所以恢复出来的还是当时那一版。
        let prompt = with_instructions(identity.instructions.as_deref(), prompt);

        let agent_surface = identity.surface.clone();

        // 资源命名空间的挂载点（§六）：skill 那一份是**这一次 Run 开始时**的活注册表快照
        // （提示目录与 `skill://` 因此同一个来源；注册表之后被重载，这一条 Run 仍按它自己
        // 的那一份，§3），会话根来自这一次的 Session，`tool://` 只读得到这次能力面。
        let mounts = ResourceMounts {
            skill_dirs: self.skill_dirs(),
            skills: self
                .skills
                .as_ref()
                .map(|skills| {
                    skills
                        .read()
                        .expect("skills 注册表")
                        .list()
                        .into_iter()
                        .filter_map(|skill| {
                            // `Skill.path` 是 SKILL.md 的字面路径；拿不到父目录就跳过这一份。
                            let dir = skill.path.parent()?.to_path_buf();
                            Some(SkillMount {
                                name: skill.name,
                                dir,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            session_root: session_root.clone(),
            tools: tools.clone().into(),
        };

        let request = TurnRequest {
            session: session.clone(),
            run: run.clone(),
            model: identity
                .model
                .clone()
                .unwrap_or_else(|| record.model.clone()),
            system_prompt: prompt,
            // **这一段对话**：上一轮说了什么、最后答了什么，下一轮必须还在。子代理是唯一
            // 的例外——它只看得见自己那条 Run，父的窗口里也没有它的过程（§4）。
            messages: match replay(
                &surface,
                scope,
                &payloads,
                self.outputs.as_deref(),
                model_result_bytes,
            )
            .await
            {
                Ok(messages) => messages,
                // 外置的正文按引用读不出来 = 会话缺内容：停下来报告（§8.3），别把一条空的
                // 用户消息当成"用户就是这么说的"发出去。
                Err(error) => return Err(self.halt_if_corrupt(&run, error).await),
            },
            tools,
            memories,
            covers: None,
        };

        let env = CallEnv {
            surface: agent_surface,
            session: session.clone(),
            run: run.clone(),
            source: record.source.clone(),
            cwd,
            roots,
            mounts,
            model_result_bytes,
            env_version: None,
            principal: None,
            // 本 Run 是被谁派的（普通 Run 是 None）。runtime 用它硬拦"子代理再委派"。
            delegated: delegate.clone(),
            // 同一个模型：`TurnRequest` 与这里用的是同一次解析的结果（冻结快照里那一个）。
            model: identity.model.unwrap_or(record.model),
            cancel: self.token_for(&run),
        };

        let budget = Budget {
            max_rounds: match &delegate {
                // 子代理的轮次预算是父侧派它时给的（§4），不是全局默认值。
                Some(spec) => spec.rounds,
                None => self.max_rounds_for(&record.source).await,
            },
            max_tokens: None,
            first_round: rounds_so_far + 1,
            retry: RetryBudget {
                attempts: record.retry_attempts,
                max_attempts: self.max_retries,
                ..RetryBudget::default()
            },
        };

        let resume = match self.resumed(&session, &surface, &events, &run).await {
            Ok(resume) => resume,
            // 外置的正文读不出来 = 会话缺内容：停下来报告（§8.3），别当成"没有可续跑的
            // 调用"——那会让这一段的请求带着一个没有输出的 `function_call` 发出去。
            Err(error) => return Err(self.halt_if_corrupt(&run, error).await),
        };

        Ok(Segment {
            session,
            run,
            request,
            env,
            budget,
            resume,
        })
    }
}

impl GatewaySegments {
    /// 这一段注入哪些记忆（§9.4、§9.7）。
    ///
    /// 查询文本是回放面上**最后一条用户消息**——「当前用户输入 + 少量任务上下文」
    /// （§9.4）。续跑时先把检查点里记的那一批交给
    /// [`MemoryManager::prepare`] 重新核对：活下来的沿用（不必再问一次 embedding），
    /// 已经遗忘或改了版本的掉出去（§9.7）。
    ///
    /// `scopes` 来自 Profile（`memory_scope`）：**过滤发生在检索里**，不是拿到结果之后再
    /// 遮掉别人 Agent 的记录（§五）。
    async fn recall_for(
        &self,
        session: &SessionId,
        run: &RunId,
        surface: &Surface,
        scopes: &[MemoryScope],
    ) -> Vec<komo_kernel::types::turn::MemoryUse> {
        let Some(memories) = self.memories.as_ref() else {
            return Vec::new();
        };
        let carried = match &self.checkpoints {
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
        // **按"这一段对话"取**，不是按这一轮重新召回（§9.4）：注入段在 system 消息里，
        // 每轮重算一次就等于每轮把服务端前缀缓存打掉一次。
        memories
            .prepare_segment(session, run, &text, &carried, surface.boundary(), scopes)
            .await
            .uses
    }

    /// 这一段的**身份与能力**（§4.3）。
    ///
    /// 三条路，按优先级：
    ///
    /// 1. **自己那条 `run.accepted` 里冻结的那一份**——正常的交互 Run 都走这条。审批可能
    ///    一小时之后才答复，那时 Profile 与磁盘都可能已经改过，恢复出来的 Run 不能因此
    ///    换一副面孔；
    /// 2. **父 Run 的快照**——子 Run 受理时拿不到 Profile、工作目录与指令正文（执行器手里
    ///    只有父的 `CallEnv`），而它本来就是父那次委派的延续：同一个 Agent、同一个目录、
    ///    同一份指令，只是不能再委派（§4 的深度只有一层，摘名字在 [`segment`] 里做）；
    /// 3. **当前配置的默认 Agent**——旧行（这次改造之前受理的）与 Cron（没有归属的入口）。
    ///    §八「已有 Session 归入默认 Agent；旧日志不重写」。
    ///
    /// **没有第四条路**：当前 Profile 不许覆盖一条已经受理的 Run。
    async fn identity_for(
        &self,
        events: &[Event],
        run: &RunId,
        session: &SessionId,
        record: Option<&komo_store::repos::session::SessionRecord>,
        catalog: &[ToolDefinition],
    ) -> Result<Identity, LedgerError> {
        let frozen = frozen_snapshot(events, run).or_else(|| {
            delegate_parent(events, run).and_then(|parent| frozen_snapshot(events, &parent))
        });
        let Some(frozen) = frozen else {
            return Ok(self.ambient_identity(record, catalog));
        };
        // 身份指令按引用读回来（§4.3：存的是内容，不是文件路径）。读不出来 = 会话缺内容，
        // 由调用方停下来报告——拿当前配置编一份提示继续跑，正是"换了一副面孔"。
        let instructions = match &frozen.instructions_ref {
            Some(reference) => {
                let bytes = self
                    .payloads_for(session)
                    .open(reference)
                    .await
                    .map_err(store_to_ledger)?;
                Some(String::from_utf8(bytes).map_err(|error| {
                    LedgerError::Corrupt(format!("冻结的身份指令不是 UTF-8：{error}"))
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
            memory_scope: frozen.memory_scope,
        })
    }

    /// 没有冻结快照时的兜底身份：**当前**配置的默认 Agent（§八）。
    ///
    /// 工作目录的取舍与受理那一步是**同一条规则**（会话 `workdir` → Profile `workspace`
    /// → `workspaces/`）：同一件事在两条路上有两个答案，正是 §八 修掉的那类缺口。
    fn ambient_identity(
        &self,
        record: Option<&komo_store::repos::session::SessionRecord>,
        catalog: &[ToolDefinition],
    ) -> Identity {
        let names: Vec<String> = catalog.iter().map(|tool| tool.name.clone()).collect();
        let config = self.config.as_ref().map(|config| config.current());
        let profile = config.as_ref().map(|config| config.agent.default_profile());
        let surface = match (profile, self.config.as_ref()) {
            (Some(profile), Some(holder)) => {
                komo_agent::surface_of(profile, &names, &holder.home().join("config.toml"))
            }
            // 精简装配（没有配置快照）：这次能用的就是目录里装着的那些。
            _ => AgentSurface::new(names),
        };
        let workspace = record
            .and_then(|record| record.workdir.clone())
            .map(PathBuf::from)
            .or_else(|| profile.and_then(|profile| profile.workspace.clone()))
            .unwrap_or_else(|| self.workspaces.clone());
        Identity {
            agent_id: profile
                .map(|profile| profile.id.clone())
                .unwrap_or_default(),
            // 模型这一格留空：调用方回落到 `runs.model_snapshot`（受理时写下的那一份）。
            model: None,
            surface,
            workspace: paths::real_root(&workspace),
            instructions: profile.and_then(|profile| profile.instructions.clone()),
            memory_scope: profile.and_then(|profile| profile.memory_scope.clone()),
        }
    }

    /// 装配读不出上下文时怎么收场。
    ///
    /// **损坏就停下来，不要放回队列**（§8.4「停止受影响会话，报告损坏」）：一个中间损坏
    /// 的会话下一次照样读不出来，而交还领取权等于让它立刻被再领一次——"领取 → 装配失败
    /// → 交还"在两秒里能转几十圈，既不前进也不停下。其余的失败照旧是 `Ledger`，调度器
    /// 交还领取权、下一轮再试。
    async fn halt_if_corrupt(&self, run: &RunId, error: LedgerError) -> HandlerError {
        let LedgerError::Corrupt(reason) = &error else {
            return HandlerError::Ledger(error);
        };
        let reason = format!("会话读不出来：{reason}");
        if let Err(problem) = self.recovery.mark_needs_attention(run, &reason).await {
            tracing::warn!(%problem, run = %run, "连 needs_attention 都写不下去");
        }
        tracing::error!(run = %run, %reason, "装配不出上下文，停止这个任务");
        HandlerError::Stopped { reason }
    }

    /// 这个 Run 还有没有没收尾的调用（§8.4 第 4 / 6 / 7 行）。
    ///
    /// 有就把它们原样交回执行器：**同一份计划、同一个调用号**，加上"这是第几次"。停在
    /// 审批上的那一个还带着它的 `approval`——`/approve` 之后续跑走的就是这条路。
    ///
    /// 它的失败只有一种：**外置的计划或参数按引用读不出来**（§8.3 要求"读取历史或恢复
    /// 调用时按引用加载需要的内容"）。那与日志读不出来是同一类——会话缺了内容，不能靠
    /// 重新领取修好，所以由调用方 [`Self::halt_if_corrupt`] 停下来报告。
    async fn resumed(
        &self,
        session: &SessionId,
        surface: &Surface,
        events: &[Event],
        run: &RunId,
    ) -> Result<Option<ResumedRound>, LedgerError> {
        // 这个 Run 得在这个会话里。
        let Some(_) = surface.runs.get(run) else {
            return Ok(None);
        };
        let waiting = waiting_approval(events, run);
        // **停在哪一次调用上由审批行回答**（`approval_requests.call_id`）：`run.waiting`
        // 只说"停在审批上"（`RunWaiting.call` 恒为 `None`），而落点在那个权威表上——在
        // 事件里编一个只会和它漂移。
        let waiting_call = match &waiting {
            Some(approval) => self
                .approvals
                .get(approval)
                .await
                .ok()
                .flatten()
                .and_then(|record| record.call),
            None => None,
        };
        let mut pending = Vec::new();
        // 外置的正文按引用读回来（§8.3）。整段共用一个 store：它只是一份路径。
        let payloads = self.payloads_for(session);
        // **这一轮还没有结果的调用，全部交回执行器**——不只是已经写过计划的那几个：
        // 一轮里前一个停下时，后面的调用连 `tool.planned` 都还没有（`Surface::open_calls`）。
        for call_id in &surface.open_calls(run) {
            // 有那一行就用它的状态；没有（连计划都还没落盘）就是 §8.4 第 6 行说的
            // "确定尚未执行"——一次尝试都还没有过。
            let (state, attempt, attempts) = match surface.calls.get(call_id) {
                Some(call) => (call.state, call.attempt.clone(), call.attempts),
                None => (ToolCallState::Planned, None, 0),
            };
            let Some(request) = call_request(&payloads, events, call_id).await? else {
                // 连承载它的那条 `message.assistant` 都找不到：账本自相矛盾，交回执行器
                // 也没有意义（它会当成"没有可恢复的调用"往下走）。
                tracing::warn!(run = %run, call = %call_id, "调用找不回原始请求，跳过续跑");
                continue;
            };
            let approval = waiting
                .as_ref()
                .filter(|_| waiting_call.as_ref() == Some(call_id))
                .cloned();
            pending.push(CallRequest {
                resumed: Some(resumed_from(state, attempt, attempts)),
                approval,
                ..request
            });
        }
        if pending.is_empty() {
            return Ok(None);
        }
        // 已经收尾的那些在回放窗口里（`Role::Tool` 的消息），不必再交一遍。
        let settled: Vec<ToolResultForModel> = Vec::new();
        // 停在审批上的调用，决定还没写下来就别再跑一遍——那会把同一个问题问第二次。
        if let Some(approval) = &waiting
            && let Ok(Some(record)) = self.approvals.get(approval).await
            && record.decision.is_none()
        {
            tracing::debug!(run = %run, approval = %approval, "审批还没有结论，这一段不续跑");
            return Ok(None);
        }
        Ok(Some(ResumedRound { settled, pending }))
    }

    /// 这个 Session 的外置正文本体（§8.3 的 `payloads/`）。
    fn payloads_for(&self, session: &SessionId) -> PayloadStore {
        PayloadStore::new(self.routed.ledgers().paths_for(session))
    }
}

/// 这一条 Run 受理时冻结下来的身份与能力（§4.3）。
///
/// 它落在**事件**里（`run.accepted`），所以重启之后读得到——这正是"恢复时按它装配"
/// 那句话的落点。
fn frozen_snapshot(events: &[Event], run: &RunId) -> Option<Box<RunSnapshot>> {
    events.iter().find_map(|event| match &event.payload {
        EventPayload::RunAccepted(body) if event.run.as_ref() == Some(run) => body.snapshot.clone(),
        _ => None,
    })
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

/// 去掉 `delegate` 的能力面（子代理那一份，§4 的深度只有一层）。
fn without_delegate(surface: AgentSurface) -> AgentSurface {
    AgentSurface::new(
        surface
            .names()
            .iter()
            .filter(|name| name.as_str() != DELEGATE_TOOL)
            .cloned(),
    )
}

/// 这一次召回放行哪些作用域（§9.2）。
///
/// `None`（Profile 没写 `memory_scope`）= **不按作用域过滤**——`RecallQuery::scopes` 空
/// 就是"全部"，与改造之前的行为一致。写了就只召回那一个，外加 [`MemoryScope::Personal`]：
/// 用户资料是**显式共享**的那一份，每个 Agent 都该看得见（否则"用户偏好深色主题"要为每个
/// Agent 各记一遍，而它们本来就都是同一个操作者的事）。
fn recall_scopes(scope: Option<&MemoryScope>) -> Vec<MemoryScope> {
    let Some(scope) = scope else {
        return Vec::new();
    };
    let mut scopes = vec![scope.clone()];
    if *scope != MemoryScope::Personal {
        scopes.push(MemoryScope::Personal);
    }
    scopes
}

/// 把身份指令放在系统提示**最前面**（§4.3），基座提示排在它后面。
fn with_instructions(instructions: Option<&str>, prompt: String) -> String {
    match instructions.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => format!("{text}\n\n{prompt}"),
        None => prompt,
    }
}

/// 这个 Run 停在哪条审批上。
///
/// §8.4 把"停"收成一个事件（`run.waiting`）加一个理由：审批是四类等待里的一类，所以这里
/// 按 `WaitReason::Approval` 挑——`retry` / `intervention` / `dependency` 那三类不是审批，
/// 续跑这一段要它们各自的条件成立（到点、答复、前一条终态），所以一律当"没停在审批上"。
///
/// **只答"哪条审批"**：停在哪个调用上是审批行自己的事（`approval_requests.call_id`），
/// 这里返不回来——调用方去查那一行。
fn waiting_approval(events: &[Event], run: &RunId) -> Option<komo_kernel::types::ids::ApprovalId> {
    events
        .iter()
        .rev()
        .filter(|event| event.run.as_ref() == Some(run))
        .find_map(|event| match &event.payload {
            EventPayload::RunWaiting(body) => match &body.reason {
                komo_kernel::types::status::WaitReason::Approval { approval } => {
                    Some(approval.clone())
                }
                _ => None,
            },
            _ => None,
        })
}

/// 从日志里把一个调用的原始请求与计划找回来。
///
/// **超限的参数与计划是外置的**（§8.3：单次参数超过 4 KiB 把包含它的模型消息正文存到
/// `payloads/`，`arguments_ref` 指向文件内的对应字段；较大的准备计划同样外置），而
/// §8.3 那句「读取历史或恢复调用时按引用加载需要的内容」说的就是这个函数。只读内联那
/// 一份会拿到**空参数**：那既不是原请求，也把一次委派的完整任务描述丢成了空对象——重
/// 新 `prepare` 必然失败，而失败的结果又是一条不落盘的结论（见 executor 的 `execute_one`）。
///
/// 计划的取法同理：内联那份是便宜的路径，大计划在 `plan_ref` 后头。§8.4 第 4 行的
/// "沿用原计划"要求的就是它——重新准备会换一个 `plan_hash`，原先那份授权就覆盖不到了
/// （§7.4）。
async fn call_request(
    payloads: &PayloadStore,
    events: &[Event],
    call: &ToolCallId,
) -> Result<Option<CallRequest>, LedgerError> {
    let Some(requested) = events.iter().rev().find_map(|event| match &event.payload {
        EventPayload::MessageAssistant(body) => body
            .tool_calls
            .iter()
            .find(|candidate| &candidate.call_id == call)
            .cloned(),
        _ => None,
    }) else {
        return Ok(None);
    };
    let arguments = match (&requested.arguments_ref, &requested.arguments) {
        (Some(reference), _) => payloads
            .open_json::<serde_json::Value>(reference)
            .await
            .map_err(store_to_ledger)?,
        (None, arguments) => arguments.clone(),
    };
    let planned = events.iter().rev().find_map(|event| match &event.payload {
        EventPayload::ToolPlanned(body) if &body.call_id == call => Some(body.clone()),
        _ => None,
    });
    let plan = match planned.map(|body| (body.plan, body.plan_ref)) {
        Some((Some(plan), _)) => Some(*plan),
        Some((None, Some(reference))) => Some(
            payloads
                .open_json::<ExecutionPlan>(&reference)
                .await
                .map_err(store_to_ledger)?,
        ),
        _ => None,
    };
    Ok(Some(CallRequest {
        call: call.clone(),
        provider_call_id: requested.provider_call_id,
        tool: requested.name,
        arguments,
        plan,
        resumed: None,
        approval: None,
    }))
}

/// 这一段回放给模型的是哪一块（§8.3）。
#[derive(Debug, Clone, Copy)]
enum ReplayScope<'a> {
    /// 主对话：最新一个 `conversation.boundary` 之后**属于主 Run** 的消息，跨 Run 聚合。
    ///
    /// 带的是**正在跑的那条 Run**：它那几句是完整协议，其余 Run 只发布"用户说了什么、
    /// 它最后答了什么"。
    Conversation(&'a RunId),
    /// 只这一条 Run（子代理，§4）：它拿不到父的对话历史，父的窗口里也没有它的过程。
    Run(&'a RunId),
}

/// 回放窗口：最新一个 `conversation.boundary` 之后的消息（§13.1 的 `/new`）。
///
/// **还没被领走的 Run 的用户消息不进窗口**（§8.4「同一 Session 后续 Run 不越过它」）。
/// 输入是接收那一刻就落盘的（§8.5 内容权威），所以它在日志里的位置**早于**当前这一轮
/// 的收尾；照搬位置发出去，provider 看到的就是"助手要了一次调用、紧接着另一个 Run 的
/// 用户消息、最后才是那次调用的输出"，直接 400（`No tool output found for tool call …`）。
/// 领取那一步已经保证后一个 Run 不会先跑（`DUE_SQL`），这里管的是它**还没跑**时那半句话。
///
/// **协议只属于正在跑的那条 Run**：历史 Run 只发布"用户说了什么（`run.accepted`）+ 它最后
/// 答了什么"，工具调用、工具结果与原生块都不进去。三件事指着同一个做法：① 历史那半轮在
/// 取消 / 失败时合法地停在"有调用、没结果"上，照搬进新请求就是上面那个 400；② 几十轮工具
/// 往返会随对话长度线性涨上下文，而它们对"接着聊"没有增量；③ 子代理跑过的那几轮本来就不
/// 属于父的对话（§4）。**上一轮说的话因此必须还在**——少了它，"去查一下"指的是哪件事就
/// 只能靠猜。
///
/// 每条工具结果的正文走的是**同一个投影函数**（`komo_kernel::projection`）：刚跑完那次与
/// 重启之后回放，必须是同一段字节。所以这里要把事实凑齐——工具名从那一轮的调用里找，
/// 完整正文、流大小与产物入口从 `output.json` 读回来；读不回来时退回账本里那 1 KiB（那是
/// "我们至少还有这些"，不是"本该如此"）。
///
/// 正文超限时是**外置**的（`text` 为 `None`，`text_ref` 指向 `payloads/`）：按引用读回来
/// 并校验哈希，读不出来就是会话缺内容（§8.3「读取历史或恢复调用时按引用加载」）。
async fn replay(
    surface: &Surface,
    scope: ReplayScope<'_>,
    payloads: &PayloadStore,
    outputs: Option<&dyn ToolOutputStore>,
    model_result_bytes: usize,
) -> Result<Vec<ReplayMessage>, LedgerError> {
    let only = match scope {
        ReplayScope::Conversation(run) | ReplayScope::Run(run) => run,
    };
    let kept = window(surface, Some(scope));

    // 历史 Run 的"它最后答了什么"是哪一条：同一个 Run 里**最后**一条带正文的 assistant。
    let mut finals: BTreeMap<&RunId, Seq> = BTreeMap::new();
    for message in &kept {
        let (Some(run), Role::Assistant) = (&message.run, message.role) else {
            continue;
        };
        if message.text.is_none() && message.text_ref.is_none() {
            continue;
        }
        let entry = finals.entry(run).or_insert(message.seq);
        *entry = (*entry).max(message.seq);
    }

    let mut out = Vec::new();
    for message in kept {
        if message.run.as_ref() != Some(only) {
            // 历史 Run：用户正文与最终回复这两句，别的都不要。
            let last = message
                .run
                .as_ref()
                .and_then(|run| finals.get(run))
                .copied();
            if message.role != Role::User && last != Some(message.seq) {
                continue;
            }
            let Some(text) = message_text(payloads, message).await? else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            out.push(ReplayMessage {
                role: message.role,
                seq: message.seq,
                text: Some(text),
                tool_calls: Vec::new(),
                tool_results: Vec::new(),
                provider_blocks: None,
            });
            continue;
        }

        let mut tool_results = Vec::new();
        for result in &message.tool_results {
            // 完整正文在 `output.json` 里；没有输出存储（精简装配）时退回账本里那 1 KiB。
            let stored = match outputs {
                Some(outputs) => outputs.open(&result.output).await.ok(),
                None => None,
            };
            let text = stored
                .as_ref()
                .and_then(|verified| verified.body.preview.as_deref())
                .map(str::to_string)
                .or_else(|| result.preview.clone());
            // 产物也在 `output.json` 里（事件那一格没有它）：同样只从落盘事实来，所以
            // "刚跑完"与回放印出的是同一份入口清单。
            let artifacts: &[komo_kernel::types::refs::ContentRef] = stored
                .as_ref()
                .map(|verified| verified.body.artifacts.as_slice())
                .unwrap_or_default();
            let facts = ToolResultFacts {
                tool: tool_name(surface, &result.call).unwrap_or("?"),
                status: result.status,
                elapsed_ms: result.elapsed_ms,
                text: text.as_deref(),
                output: &result.output,
                stdout: result.stdout.as_ref(),
                stderr: result.stderr.as_ref(),
                artifacts,
            };
            tool_results.push(ToolResultForModel {
                provider_call_id: provider_call_id(surface, &result.call)
                    .unwrap_or_else(|| result.call.to_string()),
                call_id: result.call.clone(),
                content: project(&facts, &ProjectionContext { model_result_bytes }),
                is_error: !matches!(
                    result.status,
                    komo_kernel::types::refs::ToolResultStatus::Completed
                ),
            });
        }
        out.push(ReplayMessage {
            role: message.role,
            seq: message.seq,
            text: message_text(payloads, message).await?,
            tool_calls: message.tool_calls.clone(),
            tool_results,
            provider_blocks: message.provider_blocks.clone(),
        });
    }
    Ok(out)
}

/// 一条消息的正文：内联的那份优先，没有就按引用读回来（§8.3）。
///
/// 只读内联那份会让超限的正文在回放里变成**空消息**：用户输入过 4 KiB 时，模型看到的是
/// 一个空的 user，而它该看到的是原话。外置正文的哈希由 `PayloadStore::open` 校验，对不上
/// 就是会话缺内容，不返回内容。
async fn message_text(
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

/// 回放窗口里属于**已经开跑过**的那些 Run，**按 Run 分组、Run 之间先来后到**。
///
/// 日志的顺序是**接收**的顺序（输入落盘的时机，§8.5 内容权威），不是执行的顺序：一个
/// Run 停在半轮上时，后一个 Run 的输入会落在它的调用与调用结果之间。照搬这个位置发出去，
/// provider 看到的是"助手要了一次调用、紧接着另一个 Run 的用户消息、最后才是那次调用的
/// 输出"，直接 400（`No tool output found for tool call …`）。§8.4 的次序本来就是 Run
/// 之间不越过，转写按它排：每个 Run 的那几句连在一起，Run 之间按先来后到。
///
/// 还没被领走的 Run 整个不进转写（`awaits_claim`）：它的输入已经落盘，但这一轮还没轮到
/// 它。TUI 走的是 `Surface::replay`（原样、按日志顺序），两者不是一回事——用户在界面上要
/// 立刻看到自己刚发的那句话，而模型不能在半轮中间读到它。
///
/// `scope` 挑的是**哪些 Run**（见 [`ReplayScope`]）；`None` = 全都要，记忆召回的查询文本
/// 用它——用户最后说的那句不该被另一个 Run 的输入顶掉。
fn window<'s>(surface: &'s Surface, scope: Option<ReplayScope<'_>>) -> Vec<&'s SurfaceMessage> {
    let kept: Vec<&SurfaceMessage> = surface
        .replay()
        .iter()
        .filter(|message| match scope {
            None => true,
            // 子代理与父在同一份日志里，但它们不是同一段对话：子代理跑过的那几轮不能进
            // 父的窗口，父的也没进过子代理的（§4）。
            Some(ReplayScope::Run(run)) => message.run.as_ref() == Some(run),
            Some(ReplayScope::Conversation(_)) => message
                .run
                .as_ref()
                .and_then(|run| surface.runs.get(run))
                .is_none_or(|view| view.delegate.is_none()),
        })
        .filter(|message| {
            message
                .run
                .as_ref()
                .and_then(|run| surface.runs.get(run))
                .is_none_or(|view| !view.status.awaits_claim())
        })
        .collect();

    // 每个 Run 的第一个 `seq` 就是它在对话里的位置；不在 Run 里的消息（`message.user`）
    // 按自己的 `seq` 排。
    let mut starts: BTreeMap<&RunId, Seq> = BTreeMap::new();
    for message in &kept {
        if let Some(run) = &message.run {
            let entry = starts.entry(run).or_insert(message.seq);
            *entry = (*entry).min(message.seq);
        }
    }

    let mut keyed: Vec<((Seq, Seq), &SurfaceMessage)> = kept
        .into_iter()
        .map(|message| {
            let key = match &message.run {
                Some(run) => (starts.get(run).copied().unwrap_or(Seq::ZERO), message.seq),
                None => (message.seq, message.seq),
            };
            (key, message)
        })
        .collect();
    keyed.sort_by_key(|(key, _)| *key);
    keyed.into_iter().map(|(_, message)| message).collect()
}

/// 回放面上最后一条用户消息的正文。
fn latest_user_text(surface: &Surface) -> Option<String> {
    window(surface, None)
        .into_iter()
        .rfind(|message| message.role == Role::User)
        .and_then(|message| message.text.clone())
}

/// 一条 Run 能用哪些根。
///
/// 除了工作目录，还有**这个 Session 自己的输出与产物**（只读）：模型看到的每条
/// `tool.result` 都写着"完整输出在哪"，那条路径必须真能读——否则那句提示就是一根死链
/// （§8.3「当前 Session 获授权的输出可通过 `read` 只读访问」）。
///
/// **只读靠两件事**：`writable: false` 让 §7.1 的写入规则不命中；而真正拦住写的是
/// §8.10 第 4 条——`sessions/` 在本机受保护名单里，写它一律 Deny（Deny 高于 Allow）。
///
/// 根与目标必须在同一套坐标系里：这里用 `real_root` 解掉符号链接，工具解析目标时走的也
/// 是它（macOS 上 `/var` → `/private/var`，字面前缀匹配会全落空）。
fn run_roots(
    sessions_dir: Option<&std::path::Path>,
    session: &SessionId,
    cwd: PathBuf,
    skill_dirs: &[PathBuf],
) -> Vec<WorkspaceRoot> {
    let mut roots = vec![WorkspaceRoot {
        path: cwd,
        writable: true,
        label: "workspace".into(),
    }];
    // §5.6：skill 目录是**只读根**——模型用 `read` 读 SKILL.md，`skill://` 那条路也要它们
    // 落在已授权范围里，否则每一次读 skill 都会撞上 `outside-roots` 去问人。
    for dir in skill_dirs {
        roots.push(WorkspaceRoot {
            path: paths::real_root(dir),
            writable: false,
            label: "skill".into(),
        });
    }
    let Some(sessions_dir) = sessions_dir else {
        return roots;
    };
    let paths = komo_store::SessionPaths::new(sessions_dir, session);
    for (path, label) in [
        (paths.tool_output(), "session-output"),
        (paths.artifacts(), "session-artifacts"),
    ] {
        roots.push(WorkspaceRoot {
            path: paths::real_root(&path),
            writable: false,
            label: label.into(),
        });
    }
    roots
}

/// 结果要按 provider 自己的 call_id 回传（§6）。
fn provider_call_id(surface: &Surface, call: &ToolCallId) -> Option<String> {
    tool_call(surface, call).map(|call| call.provider_call_id.clone())
}

/// 这个调用是哪个工具发的——投影的抬头要它，而事件里只记了调用号。
fn tool_name<'s>(surface: &'s Surface, call: &ToolCallId) -> Option<&'s str> {
    tool_call(surface, call).map(|call| call.name.as_str())
}

/// 这一轮里的那条调用。工具名与 provider 的 call_id 都在它上面。
fn tool_call<'s>(surface: &'s Surface, call: &ToolCallId) -> Option<&'s ToolCallRequest> {
    surface.messages.iter().rev().find_map(|message| {
        message
            .tool_calls
            .iter()
            .find(|candidate| &candidate.call_id == call)
    })
}

/// 提示里每条 Run 都说的那几句行为约束。
///
/// **一处定义**：主对话与子代理两份提示各抄一份就会漂，而漂掉的那一条恰恰是模型真正照
/// 着做的那一条。
const RULES: &str = "找代码和文字用 rg 工具；不要用 shell 里的 grep / find / ls 拼搜索。\n\
                     危险操作会被拦下来等人批准；被拒绝就把它当作结果，不要绕过。\n\
                     做完之后如实报告做了什么、有什么证据；没有证据就说没有。";

/// 系统提示的正文：身份 / 工作目录 / 挂着的工具 / §5.6 的 skills 目录 / 三条行为约束。
///
/// `skills` 是 [`GatewaySegments::skills_block`] 从**活注册表**现渲染出来的那一块
/// （§5.6：同一份注册表，`skill://` 的挂载点也抄它）；没有能露面的 skills 时它是空串。
fn system_prompt(cwd: &std::path::Path, tools: &[ToolDefinition], skills: &str) -> String {
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    let mut prompt = format!(
        "你是 komo，一个在用户自己机器上运行的助手。\n\
         工作目录：{}\n\
         可用工具：{}\n\
         {RULES}",
        cwd.display(),
        if names.is_empty() {
            "（这一段没有工具）".to_string()
        } else {
            names.join("、")
        }
    );
    if !skills.trim().is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(skills);
    }
    prompt
}

/// 子代理的系统提示（§4）。
///
/// **它不共享父那份正文**：子代理拿不到父的对话历史、记忆与 Skills，只拿得到这一条任务。
/// 所以提示必须自己把三件事说清：这是被派出来的、干完要交什么、以及"任务里没写的东西
/// 就是没给你"——最后这句不是客套，它把"自包含"从一句设计口号变成对子代理可执行的要求。
fn subagent_prompt(cwd: &std::path::Path, tools: &[ToolDefinition], spec: &DelegateSpec) -> String {
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    let mut prompt = format!(
        "你是 komo 派出去的子代理，只负责下面这一件事。你看不到主对话、记忆与 Skills——\
         任务里没写的上下文就是没有给你，需要什么就用工具自己查，查不到就如实说。\n\
         任务：{}\n\
         工作目录：{}\n\
         可用工具：{}\n\
         {RULES}",
        spec.task,
        cwd.display(),
        if names.is_empty() {
            "（这一段没有工具）".to_string()
        } else {
            names.join("、")
        }
    );
    prompt.push_str("\n\n最后一条回复要**只有结果本身**（不要在那里调工具、不要寒暄）：");
    match &spec.contract {
        Some(contract) => {
            prompt.push_str(
                "\n一个 JSON 对象，满足下面这份 schema——父侧会用**同一份**校验，\
                 不合规会被退回来让你改：\n",
            );
            prompt.push_str(
                &serde_json::to_string_pretty(&contract.schema).unwrap_or_else(|_| "{}".into()),
            );
        }
        None => prompt.push_str("\n一段能独立读懂的结论（父侧只会把这段文本拿走）。"),
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::events::MessageAssistant;
    use komo_kernel::types::ids::EventId;

    fn event(seq: u64, run: &RunId, payload: EventPayload) -> Event {
        Event {
            v: 1,
            seq: Seq(seq),
            event_id: EventId::from_raw(format!("evt-{seq}")),
            session: SessionId::from_raw("sess-1"),
            run: Some(run.clone()),
            ts: time::macros::datetime!(2026-09-16 08:00:00 UTC),
            payload,
        }
    }

    #[tokio::test]
    async fn a_call_request_comes_back_with_its_provider_id_and_arguments() {
        let run = RunId::from_raw("run-1");
        let call = ToolCallId::from_raw("call-1");
        let events = vec![event(
            1,
            &run,
            EventPayload::MessageAssistant(MessageAssistant {
                round: 1,
                text: None,
                text_ref: None,
                tool_calls: vec![komo_kernel::types::turn::ToolCallRequest {
                    call_id: call.clone(),
                    provider_call_id: "pc-7".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "a.txt"}),
                    arguments_ref: None,
                }],
                provider_blocks: None,
                input_tokens: None,
                output_tokens: None,
            }),
        )];
        let payloads = payloads();
        let request = call_request(&payloads, &events, &call)
            .await
            .expect("读得出来")
            .expect("找得回来");
        assert_eq!(request.provider_call_id, "pc-7");
        assert_eq!(request.tool, "read");
        assert_eq!(request.arguments["path"], "a.txt");
        assert!(request.plan.is_none(), "还没有计划落盘");
    }

    /// 参数与计划超限时是**外置**的（§8.3）：找回来时按引用读回来。
    ///
    /// 只读内联那一份会拿到空参数——那既不是原请求，也让这次恢复的 `prepare` 必然失败。
    #[tokio::test]
    async fn an_externalized_argument_and_plan_come_back_by_reference() {
        let run = RunId::from_raw("run-1");
        let call = ToolCallId::from_raw("call-1");
        let session = SessionId::from_raw("sess-1");
        let payloads = payloads();

        let arguments = serde_json::json!({ "task": "一段很长很长很长很长的任务".repeat(300) });
        let arguments_ref = payloads
            .put_json(&arguments, Some("/tool_calls/0/arguments".into()))
            .await
            .expect("外置得进去");
        let plan = komo_kernel::test_support::sample_plan("delegate", &session);
        let plan_ref = payloads
            .put_json(&plan, Some("/plan".into()))
            .await
            .expect("外置得进去");

        let events = vec![
            event(
                1,
                &run,
                EventPayload::MessageAssistant(MessageAssistant {
                    round: 1,
                    text: None,
                    text_ref: None,
                    tool_calls: vec![komo_kernel::types::turn::ToolCallRequest {
                        call_id: call.clone(),
                        provider_call_id: "pc-7".into(),
                        name: "delegate".into(),
                        arguments: serde_json::Value::Null,
                        arguments_ref: Some(arguments_ref),
                    }],
                    provider_blocks: None,
                    input_tokens: None,
                    output_tokens: None,
                }),
            ),
            event(
                2,
                &run,
                EventPayload::ToolPlanned(komo_kernel::events::ToolPlanned {
                    call_id: call.clone(),
                    plan_hash: plan.plan_hash(),
                    plan: None,
                    plan_ref: Some(plan_ref),
                }),
            ),
        ];
        let request = call_request(&payloads, &events, &call)
            .await
            .expect("读得出来")
            .expect("找得回来");
        assert_eq!(request.arguments, arguments, "参数要按引用读回来");
        assert_eq!(
            request.plan.as_ref().map(ExecutionPlan::plan_hash),
            Some(plan.plan_hash()),
            "计划也要按引用读回来：重新准备会换掉 plan_hash（§7.4）"
        );
    }

    fn payloads() -> PayloadStore {
        let dir = tempfile::tempdir().expect("临时目录");
        PayloadStore::new(komo_store::SessionPaths::at(dir.keep()))
    }

    // ---------------------------------------------------------------- 回放窗口（§8.3）

    use komo_kernel::types::delegate::DelegateSpec;
    use komo_kernel::types::digest::ContentHash;
    use komo_kernel::types::ids::{ExecutorId, RequestKey};
    use komo_kernel::types::plan::PlanSource;
    use komo_kernel::types::refs::PayloadRef;

    fn accepted(run: &RunId, seq: u64, text: &str) -> Event {
        accepted_as(run, seq, Some(text), None, None)
    }

    fn accepted_as(
        run: &RunId,
        seq: u64,
        text: Option<&str>,
        text_ref: Option<PayloadRef>,
        delegate: Option<DelegateSpec>,
    ) -> Event {
        event(
            seq,
            run,
            EventPayload::RunAccepted(komo_kernel::events::RunAccepted {
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
                delegate,
                snapshot: None,
            }),
        )
    }

    fn assistant(
        run: &RunId,
        seq: u64,
        text: Option<&str>,
        calls: Vec<ToolCallRequest>,
        blocks: Option<serde_json::Value>,
    ) -> Event {
        event(
            seq,
            run,
            EventPayload::MessageAssistant(MessageAssistant {
                round: seq as u32,
                text: text.map(str::to_string),
                text_ref: None,
                tool_calls: calls,
                provider_blocks: blocks,
                input_tokens: None,
                output_tokens: None,
            }),
        )
    }

    fn started(run: &RunId, seq: u64) -> Event {
        event(
            seq,
            run,
            EventPayload::RunStarted(komo_kernel::events::RunStarted {
                executor: ExecutorId::from_raw("exec-1"),
                generation: 1,
            }),
        )
    }

    fn request(call: &ToolCallId) -> ToolCallRequest {
        ToolCallRequest {
            call_id: call.clone(),
            provider_call_id: "pc-1".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "a.txt"}),
            arguments_ref: None,
        }
    }

    /// 上一轮说了什么、最后答了什么，下一轮必须还在。
    ///
    /// 只按本 Run 过滤时，用户那句"去查一下"落地的时候上一轮的问答已经不在窗口里了：模型
    /// 只能靠猜（真实会话里它去读了 `config.toml`，然后答了一个没人问过的问题）。
    #[tokio::test]
    async fn a_later_run_still_reads_what_the_earlier_one_said() {
        let first = RunId::from_raw("run-1");
        let second = RunId::from_raw("run-2");
        let call = ToolCallId::from_raw("call-1");
        let events = vec![
            accepted(&first, 1, "捞一下线上订单在 loong 请求了哪些接口，先查 db"),
            started(&first, 2),
            // 一轮工具往返：**历史 Run 里它不该再进新请求**——协议只属于正跑的那条 Run。
            assistant(
                &first,
                3,
                None,
                vec![request(&call)],
                Some(serde_json::json!([{"type": "reasoning"}])),
            ),
            assistant(
                &first,
                4,
                Some("SQL 如下：SELECT 1。确认执行吗？"),
                vec![],
                Some(serde_json::json!([{"type": "message"}])),
            ),
            event(
                5,
                &first,
                EventPayload::RunCompleted(komo_kernel::events::RunCompleted {
                    final_message: None,
                    final_message_ref: None,
                    rounds: 2,
                }),
            ),
            accepted(&second, 6, "去查一下"),
            started(&second, 7),
        ];
        let surface = fold(&events);
        let messages = replay(
            &surface,
            ReplayScope::Conversation(&second),
            &payloads(),
            None,
            8 * 1024,
        )
        .await
        .expect("读得出来");

        let seen: Vec<(Role, Option<&str>)> = messages
            .iter()
            .map(|message| (message.role, message.text.as_deref()))
            .collect();
        assert_eq!(
            seen,
            vec![
                (
                    Role::User,
                    Some("捞一下线上订单在 loong 请求了哪些接口，先查 db")
                ),
                (Role::Assistant, Some("SQL 如下：SELECT 1。确认执行吗？")),
                (Role::User, Some("去查一下")),
            ],
            "上一轮的问答要跟着进新 Run，而它那轮工具往返不进"
        );
        for message in &messages {
            assert!(message.tool_calls.is_empty(), "{message:?}");
            assert!(message.tool_results.is_empty(), "{message:?}");
            assert!(message.provider_blocks.is_none(), "{message:?}");
        }
    }

    /// 产物在**回放那一侧**同样印出来：入口与大小从 `output.json` 读回来（事件里没有那一格），
    /// 所以"刚跑完"与"重启之后回放"给模型的是同一份入口清单（§4.7、§8.3）。
    #[tokio::test]
    async fn a_replayed_round_names_the_artifacts_it_produced() {
        use komo_kernel::test_support::{MemOutputStore, MemOutputWriter};
        use komo_kernel::types::ids::AttemptId;
        use komo_kernel::types::refs::{AttemptRef, ContentRef, ToolResultBody};

        let run = RunId::from_raw("run-1");
        let call = ToolCallId::from_raw("call-1");
        let attempt = AttemptId::from_raw("attempt-1");
        let artifacts = vec![ContentRef {
            path: "artifacts/run-1/报告.md".into(),
            size: "产物正文\n".len() as u64,
            hash: ContentHash::of_str("产物正文\n"),
            pointer: None,
        }];

        let outputs = MemOutputStore::new();
        let writer = MemOutputWriter::new(AttemptRef {
            session: SessionId::from_raw("sess-1"),
            run: run.clone(),
            call: call.clone(),
            attempt: attempt.clone(),
        });
        let published = outputs
            .publish(
                Box::new(writer),
                ToolResultBody {
                    status: komo_kernel::types::refs::ToolResultStatus::Completed,
                    result: serde_json::json!({ "ok": true }),
                    error: None,
                    exit_code: Some(0),
                    artifacts,
                    preview: Some("写完了一份报告\n".into()),
                },
            )
            .await
            .expect("发布这次尝试的输出");

        let events = vec![
            accepted(&run, 1, "写一份报告"),
            started(&run, 2),
            assistant(&run, 3, None, vec![request(&call)], None),
            event(
                4,
                &run,
                EventPayload::ToolResult(komo_kernel::events::ToolResult {
                    call_id: call.clone(),
                    attempt_id: attempt,
                    status: komo_kernel::types::refs::ToolResultStatus::Completed,
                    output_ref: published.output.clone(),
                    elapsed_ms: published.elapsed_ms,
                    preview: published.preview.clone(),
                    stdout: published.stdout.clone(),
                    stderr: published.stderr.clone(),
                    attempt_state: None,
                }),
            ),
        ];
        let surface = fold(&events);
        let messages = replay(
            &surface,
            ReplayScope::Conversation(&run),
            &payloads(),
            Some(&outputs),
            8 * 1024,
        )
        .await
        .expect("读得出来");

        let content = &messages
            .iter()
            .flat_map(|message| message.tool_results.iter())
            .next()
            .expect("回放里有工具结果")
            .content;
        assert!(content.contains("写完了一份报告"), "{content}");
        assert!(
            content.contains("产物：artifact://files/run-1/报告.md（13 B）"),
            "{content}"
        );
        assert!(
            !content.contains("产物正文"),
            "产物的正文按引用去读，回放也不抄：{content}"
        );
    }

    /// 正跑着的那条 Run 仍然是**完整协议**：它自己那轮的调用、结果与原生块一个都不能少。
    #[tokio::test]
    async fn the_running_run_keeps_its_whole_protocol() {
        let run = RunId::from_raw("run-1");
        let call = ToolCallId::from_raw("call-1");
        let events = vec![
            accepted(&run, 1, "看一下 a.txt"),
            started(&run, 2),
            assistant(
                &run,
                3,
                None,
                vec![request(&call)],
                Some(serde_json::json!([{"type": "reasoning"}])),
            ),
        ];
        let surface = fold(&events);
        let messages = replay(
            &surface,
            ReplayScope::Conversation(&run),
            &payloads(),
            None,
            8 * 1024,
        )
        .await
        .expect("读得出来");

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].tool_calls.len(), 1);
        assert_eq!(
            messages[1].tool_calls[0].call_id, call,
            "调用要原样交给 provider"
        );
        assert_eq!(
            messages[1].provider_blocks,
            Some(serde_json::json!([{"type": "reasoning"}])),
            "原生块逐字回放（§13.2）"
        );
    }

    /// 子代理只看得见自己那条 Run，父的窗口里也没有它的过程（§4）。
    #[tokio::test]
    async fn a_subagent_and_its_parent_are_two_different_conversations() {
        let parent = RunId::from_raw("run-parent");
        let child = RunId::from_raw("run-child");
        let call = ToolCallId::from_raw("call-1");
        let spec = DelegateSpec::new(parent.clone(), call.clone(), "看一下这个 PR");
        let events = vec![
            accepted(&parent, 1, "父的输入"),
            started(&parent, 2),
            accepted_as(&child, 3, Some("看一下这个 PR"), None, Some(spec)),
            started(&child, 4),
            assistant(&child, 5, Some("子代理的过程"), vec![], None),
            assistant(&parent, 6, Some("父的回复"), vec![], None),
        ];
        let surface = fold(&events);
        let payloads = payloads();

        let of = |scope| {
            let surface = &surface;
            let payloads = &payloads;
            async move {
                replay(surface, scope, payloads, None, 8 * 1024)
                    .await
                    .expect("读得出来")
                    .into_iter()
                    .map(|message| message.text)
                    .collect::<Vec<_>>()
            }
        };

        assert_eq!(
            of(ReplayScope::Conversation(&parent)).await,
            vec![Some("父的输入".into()), Some("父的回复".into())],
            "父的窗口里不该有子代理的过程——它只该拿到那条结果（§4）"
        );
        assert_eq!(
            of(ReplayScope::Run(&child)).await,
            vec![Some("看一下这个 PR".into()), Some("子代理的过程".into())],
            "子代理拿不到父的对话历史"
        );
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
            accepted_as(&first, 1, None, Some(question), None),
            started(&first, 2),
            // 一条没有正文的轮次：它不该抢走"最后答了什么"的位置。
            assistant(&first, 3, None, vec![], None),
            event(
                4,
                &first,
                EventPayload::MessageAssistant(MessageAssistant {
                    round: 2,
                    text: None,
                    text_ref: Some(reply),
                    tool_calls: vec![],
                    provider_blocks: None,
                    input_tokens: None,
                    output_tokens: None,
                }),
            ),
            accepted(&second, 5, "接着上面那句"),
            started(&second, 6),
        ];
        let surface = fold(&events);
        let messages = replay(
            &surface,
            ReplayScope::Conversation(&second),
            &payloads,
            None,
            8 * 1024,
        )
        .await
        .expect("读得出来");

        assert_eq!(
            messages[0].text.as_deref(),
            Some(big.as_str()),
            "外置的用户输入要按引用读回来"
        );
        assert_eq!(
            messages[1].text.as_deref(),
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
        let events = vec![
            accepted_as(&run, 1, None, Some(missing), None),
            started(&run, 2),
        ];
        let surface = fold(&events);
        let error = replay(
            &surface,
            ReplayScope::Conversation(&run),
            &payloads(),
            None,
            8 * 1024,
        )
        .await
        .expect_err("读不出来就不该装作读到了");
        assert!(matches!(error, LedgerError::Corrupt(_)), "{error:?}");
    }

    #[test]
    fn the_waiting_approval_is_the_latest_one() {
        let run = RunId::from_raw("run-1");
        let waiting = |approval: &str| {
            EventPayload::RunWaiting(komo_kernel::events::RunWaiting {
                reason: komo_kernel::types::status::WaitReason::Approval {
                    approval: komo_kernel::types::ids::ApprovalId::from_raw(approval),
                },
            })
        };
        let events = vec![
            event(1, &run, waiting("ap-1")),
            event(2, &run, waiting("ap-2")),
        ];
        // 最后一条说了算：中途答过的那一条不是"现在停在哪儿"。
        assert_eq!(
            waiting_approval(&events, &run)
                .expect("停在审批上")
                .as_str(),
            "ap-2"
        );
    }

    /// **另外三类等待不是审批**：`retry` / `intervention` / `dependency` 停在同一个
    /// `run.waiting` 上，但各自的放行条件完全不是"有人批了这份计划"——把它们读成审批，
    /// 续跑那一段就会拿着一条不存在的授权往下跑（§8.4、§7.5 第 3 条）。
    #[test]
    fn waiting_on_a_clock_or_a_person_is_not_a_waiting_approval() {
        let run = RunId::from_raw("run-1");
        let reason = |reason: komo_kernel::types::status::WaitReason| {
            EventPayload::RunWaiting(komo_kernel::events::RunWaiting { reason })
        };
        let events = vec![
            event(
                1,
                &run,
                reason(komo_kernel::types::status::WaitReason::Retry {
                    attempts: 2,
                    not_before: time::macros::datetime!(2026-09-16 08:00:00 UTC),
                    cause: komo_kernel::types::status::RetryCause::RateLimited,
                }),
            ),
            event(
                2,
                &run,
                reason(komo_kernel::types::status::WaitReason::Dependency {
                    run: RunId::from_raw("run-0"),
                }),
            ),
            event(
                3,
                &run,
                reason(komo_kernel::types::status::WaitReason::Intervention {
                    intervention: komo_kernel::types::ids::InterventionId::from_raw("run-1"),
                }),
            ),
        ];
        assert_eq!(waiting_approval(&events, &run), None);
    }

    #[test]
    fn the_system_prompt_names_the_tools_that_are_actually_mounted() {
        let prompt = system_prompt(
            std::path::Path::new("/tmp/w"),
            &[ToolDefinition {
                name: "read".into(),
                description: "读文件".into(),
                parameters: serde_json::json!({}),
            }],
            "",
        );
        assert!(prompt.contains("read"), "{prompt}");
        assert!(prompt.contains("/tmp/w"), "{prompt}");
    }

    /// §5.6 的目录行拼在**后面**（原来那段正文一句不少），空的时候一个字都不多。
    #[test]
    fn the_system_prompt_carries_the_skills_catalog_after_the_base_text() {
        let tools = [ToolDefinition {
            name: "read".into(),
            description: "读文件".into(),
            parameters: serde_json::json!({}),
        }];
        let block = "Skills（人写的操作说明…）：\n- pr-review：怎么审一个 PR";
        let prompt = system_prompt(std::path::Path::new("/tmp/w"), &tools, block);
        assert!(prompt.ends_with(block), "{prompt}");
        assert!(prompt.contains("你是 komo"), "{prompt}");

        let bare = system_prompt(std::path::Path::new("/tmp/w"), &tools, "");
        assert!(!bare.contains("Skills"), "{bare}");
        assert_eq!(bare.lines().count(), 6, "{bare}");
    }

    /// 「搜代码用 rg，不要用 shell 里的 grep / find / ls」是**两条提示共用**的一句
    /// （[`RULES`]）：子代理也要照它做，所以它不能只写在主对话那一份里。
    #[test]
    fn both_prompts_tell_the_model_to_search_with_rg() {
        let tools = [ToolDefinition {
            name: "rg".into(),
            description: "搜".into(),
            parameters: serde_json::json!({}),
        }];
        let main = system_prompt(std::path::Path::new("/tmp/w"), &tools, "");
        assert!(main.contains("可用工具：rg"), "{main}");
        assert!(main.contains("不要用 shell 里的 grep"), "{main}");

        let spec = komo_kernel::types::delegate::DelegateSpec::new(
            RunId::from_raw("run-1"),
            ToolCallId::from_raw("call-1"),
            "查一下调用方",
        );
        let sub = subagent_prompt(std::path::Path::new("/tmp/w"), &tools, &spec);
        assert!(sub.contains("不要用 shell 里的 grep"), "{sub}");
    }

    /// §8.3：这个 Session 自己的输出与产物是**只读**根——模型看到的"完整输出在哪"得真能
    /// 读进去，而写进去不许命中"范围内写入"那条 Allow。§5.6：skill 目录同样是只读根。
    #[test]
    fn a_run_can_read_its_own_session_output_and_skills_but_not_write_them() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::from_raw("sess-1");
        let skills = dir.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        let roots = run_roots(
            Some(dir.path()),
            &session,
            PathBuf::from("/tmp/w"),
            std::slice::from_ref(&skills),
        );

        let labels: Vec<&str> = roots.iter().map(|root| root.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["workspace", "skill", "session-output", "session-artifacts"],
            "工作目录可写，skill 与 Session 自己的两处只读"
        );
        assert!(roots[0].writable);

        let skill = roots.iter().find(|root| root.label == "skill").unwrap();
        assert!(!skill.writable, "skill 目录是只读根（§5.6）");
        assert_eq!(skill.path, std::fs::canonicalize(&skills).unwrap());

        let output = roots
            .iter()
            .find(|root| root.label == "session-output")
            .unwrap();
        assert!(!output.writable);
        assert!(
            output.path.ends_with("sess-1/tool-output"),
            "{}",
            output.path.display()
        );
        assert!(output.path.is_absolute(), "根必须是绝对路径");
        let artifacts = roots
            .iter()
            .find(|root| root.label == "session-artifacts")
            .unwrap();
        assert!(artifacts.path.ends_with("sess-1/artifacts"));

        // 没接 `sessions/` 的精简装配只有工作目录与 skill 根——不该凭空多出两个根。
        let bare = run_roots(None, &session, PathBuf::from("/tmp/w"), &[skills]);
        assert_eq!(
            bare.iter()
                .map(|root| root.label.as_str())
                .collect::<Vec<_>>(),
            vec!["workspace", "skill"]
        );

        // 一份 skill 都没接时不多不少——工作目录一条。
        let none = run_roots(None, &session, PathBuf::from("/tmp/w"), &[]);
        assert_eq!(none.len(), 1);
    }
}
