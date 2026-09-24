//! 领到一个 Run 之后，这一段要的上下文从哪来（`SegmentSource` 的生产实现）。
//!
//! **模型看见什么由 `komo-agent::context` 装配**（`docs/agent.md` §5、§17）；这里只做
//! 三件"为执行取数"的事：
//!
//! - **工作目录与已授权根**：Session 的 `workdir`，没有就是 `workspaces/`。
//! - **恢复位置**：这个 Run 还有没有没收尾的调用。有就把它们原样交回执行器——**沿用
//!   同一份计划**，因为审批绑定的是计划的哈希，重新 prepare 会换一个哈希（§7.4）。
//! - **`CallEnv` 的根与挂载**（`ResourceMounts`）。
//!
//! `segment()` 自己的形状是 §17 那五步：`load → identity → tools → resolve ContextInput
//! → assemble → 组装 TurnRequest / CallEnv / Budget / resume / command`。取数的那一半
//! （身份、记忆召回、history 的 I/O、skills 目录读取）在 `context_sources.rs`；"模型该
//! 看见什么"（系统提示、回放消息）在 `komo_agent::context`。
//!
//! **记忆在这里召回**（§9.4）：这一段装配时按最新一句用户输入召回一次，渲染出的正文
//! （`komo_agent::context::memory::render`，`docs/agent.md` §13.2）经 `ContextInput.memory`
//! 交给 `assemble`，放在系统提示的最后一段——`TurnRequest.system_prompt` 就是实际发出去
//! 的那份，适配器不再各自追加。用到的条目连同它们的 revision 写进
//! [`TurnRequest::memories`]——那是审计证据，resume 时要按它重新核对（§9.7）。续跑时先拿
//! 检查点里记的那一批去核对，**过期或已遗忘的复活不了**。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_agent::DELEGATE_TOOL;
use komo_agent::context::history::{self, ReplayScope};
use komo_agent::context::{ContextInput, InvocationContext, assemble};
use komo_kernel::events::{Event, EventPayload};
use komo_kernel::fold::fold;
use komo_kernel::traits::{ApprovalRepo, Ledger, LedgerError, ToolOutputStore};
use komo_kernel::types::ids::{RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::plan::ExecutionPlan;
use komo_kernel::types::resource::{ResourceMounts, SkillMount};
use komo_kernel::types::status::ToolCallState;
use komo_kernel::types::surface::AgentSurface;
use komo_kernel::types::tool::{CancelToken, ToolDefinition, WorkspaceRoot};
use komo_kernel::types::turn::{ToolResultForModel, TurnRequest};
use komo_runtime::agent::handler::SegmentSource;
use komo_runtime::agent::{Budget, ResumedRound, RetryBudget, Segment};
use komo_runtime::config::ConfigHolder;
use komo_runtime::executor::{CallEnv, CallRequest, ToolExecutor, resumed_from};
use komo_runtime::memory::MemoryManager;
use komo_runtime::scheduler::HandlerError;
use komo_runtime::tools::paths;
use komo_store::db::store_to_ledger;
use komo_store::{CheckpointStore, Db, PayloadStore, RecoveryStore};

use super::context_sources;
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
    /// 提示里的那一块目录行**每次装配现渲染**（`context_sources::skill_catalog`），
    /// `skill://` 的挂载点也从这里抄——两处同一个来源，"提示里看见的名字读不到"就不是
    /// 一种可能了。
    skills: Option<Arc<std::sync::RwLock<komo_agent::skills::SkillRegistry>>>,
    /// 执行器。**只用来渲染交给模型的工具 Schema**（[`ToolExecutor::definitions_for`]）：
    /// 能力面已经由冻结快照（或它的兜底）给出，"这次能用哪些工具"的判据只有那一处，
    /// 这里不该再有一份。`None` = 精简装配（没有执行器的那几个单元测试）。
    executor: Option<Arc<ToolExecutor>>,
    /// 正在跑的 komo 可执行文件：主 Agent 的提示里要告诉模型 komo 自己的状态用它查。
    /// 构造时取一次，进程内不变（提示前缀稳定）。
    komo_exe: Option<PathBuf>,
}

impl std::fmt::Debug for GatewaySegments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewaySegments").finish_non_exhaustive()
    }
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
            komo_exe: None,
        }
    }

    /// 告诉模型 komo 自己的可执行文件在哪（`std::env::current_exe()`；取不到就不说）。
    pub fn with_komo_exe(mut self, exe: Option<PathBuf>) -> Self {
        self.komo_exe = exe;
        self
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

    /// 这一段是不是**命令直跑模式**（§10）：命令 Job 触发的 Run 不经模型，`AgentLoop`
    /// 用 [`komo_runtime::agent::command_driver::CommandDriver`] 代替 `LlmClient`。
    ///
    /// 判据只看 Job 定义（`command.is_some()`），不看别的：子代理不会由命令 Job 派生
    /// （`CommandDriver` 从不发出 `delegate` 调用），但仍然显式排掉 `delegate.is_some()`
    /// ——这一段要装配的是父 Run 继续走模型，不是意外地换成命令驱动。
    async fn command_for(
        &self,
        source: &komo_kernel::types::plan::PlanSource,
        is_delegate: bool,
    ) -> Option<komo_runtime::agent::command_driver::CommandSpec> {
        if is_delegate {
            return None;
        }
        let komo_kernel::types::plan::PlanSource::Cron { job, .. } = source else {
            return None;
        };
        let cron = self.cron.as_ref()?;
        match cron.get(job).await {
            Ok(Some(job)) => job
                .command
                .map(|command| komo_runtime::agent::command_driver::CommandSpec { command }),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%error, %job, "读不出这个 Job，按普通模型 Run 装配");
                None
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
        let mut identity = match context_sources::identity_for(
            &events,
            &run,
            &payloads,
            context_sources::Fallback {
                config: self.config.as_ref(),
                record: session_record.as_ref(),
                workspaces: &self.workspaces,
            },
            &catalog,
        )
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
        // 子代理这条线（旧→新，含正在跑的这一条）：顺着 `resumes` 往回找。它决定回放窗口
        // （下面的 `scope`），不在 `delegate.is_some()` 之外再多问一次账本——线是从 JSONL
        // 里 fold 出来的（§4、§8.3）。
        let thread = delegate
            .as_ref()
            .map(|_| history::delegate_thread(&surface, &run));

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
        let injection = match &delegate {
            Some(_) => komo_kernel::types::memory::Injection::default(),
            // 记忆作用域来自 Profile（§9.2）：只召回这一个作用域，外加显式共享的用户资料。
            None => {
                let scopes = context_sources::recall_scopes(identity.memory_scope.as_ref());
                context_sources::recall_for(
                    self.memories.as_ref(),
                    self.checkpoints.as_ref(),
                    &session,
                    &run,
                    &surface,
                    &scopes,
                )
                .await
            }
        };

        // 回放给模型的是**这一段对话**，不是这一条 Run（§8.3）。子代理的"这一段对话"
        // 就是它那条线（§4 的 `resumes` 链）。
        let scope = match &thread {
            Some(chain) => ReplayScope::Thread(chain),
            None => ReplayScope::Conversation(&run),
        };

        // 交给模型的 Schema 是**能力面**渲染出来的（§4 末）：一个来源，两处用——执行器查的
        // 是同一份 `AgentSurface`（`CallEnv::surface`）。两边各拿一份就会出现"schema 里没有
        // 这个名字、执行器却查得到"。tool definitions 要先算好——系统提示要列工具名，skills
        // 门控也按它（§16）。
        let tools = match &self.executor {
            Some(executor) => executor.definitions_for(&identity.surface),
            // 精简装配（没有执行器）：目录里这一段允许的那些就是 Schema——判据还是那一份
            // 能力面，不是又一句"这次能用什么"。
            None => catalog
                .into_iter()
                .filter(|tool| identity.surface.allows(&tool.name))
                .collect(),
        };
        let tool_names: Vec<String> = tools.iter().map(|tool| tool.name.clone()).collect();

        // §5.6 的 skills 目录只有主 Agent 有（§12：子代理不列目录，能力隔离靠上游，不靠
        // 提示词）。
        let skills = match &delegate {
            Some(_) => None,
            None => context_sources::skill_catalog(self.skills.as_deref(), &tool_names),
        };

        // 任务看板只有分发器 Run 才有（`docs/home-dispatcher.md` §5）：`identity.tasks`
        // 已经在 `identity_for` 里按冻结快照的 `dispatcher_tasks_ref` 读回来了——分发器
        // 从不委派（Profile 只有 `dispatch` / `follow` 两个工具），这里再挡一道子代理
        // 只是防御性的（与 `skills` 同理，§12：能力隔离靠上游，不靠提示词）。
        let tasks = match &delegate {
            Some(_) => None,
            None => identity.tasks.clone(),
        };

        // history 的纯逻辑先选窗口（`komo-agent`），再对**选中的**条目读正文与
        // `output.json`（`context_sources`，§8）。
        let selected = history::entries(&surface, scope);
        let resolved =
            match context_sources::resolve_history(selected, &payloads, self.outputs.as_deref())
                .await
            {
                Ok(resolved) => resolved,
                // 外置的正文按引用读不出来 = 会话缺内容：停下来报告（§8.3），别把一条空的
                // 用户消息当成"用户就是这么说的"发出去。
                Err(error) => return Err(self.halt_if_corrupt(&run, error).await),
            };

        let invocation = match &delegate {
            Some(spec) => InvocationContext::Delegated(spec.clone()),
            None => InvocationContext::Main,
        };

        // **唯一的 Context Assembly 入口**（§5）：Gateway 到这里为止只是"凑齐事实"，
        // "模型这一刻看见什么"由它一家决定——记忆段放在哪也是它说了算（§13.2）。
        let context = assemble(ContextInput {
            instructions: identity.instructions.clone(),
            workspace: cwd.clone(),
            tools: tool_names,
            history: resolved,
            memory: injection.text.clone(),
            skills,
            tasks,
            invocation,
            komo_exe: self.komo_exe.clone(),
            model_result_bytes,
        });

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
            system_prompt: context.system_prompt,
            // **这一段对话**：上一轮说了什么、最后答了什么，下一轮必须还在。子代理是唯一
            // 的例外——它只看得见自己那条 Run，父的窗口里也没有它的过程（§4）。
            messages: context.messages,
            tools,
            memories: injection.uses,
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

        let command = self.command_for(&record.source, delegate.is_some()).await;

        Ok(Segment {
            session,
            run,
            request,
            env,
            budget,
            resume,
            command,
        })
    }
}

impl GatewaySegments {
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
        surface: &komo_kernel::fold::Surface,
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

#[cfg(test)]
#[path = "segment_golden.rs"]
mod golden;

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::events::MessageAssistant;
    use komo_kernel::types::ids::EventId;
    use komo_kernel::types::turn::ToolCallRequest;

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

    fn payloads() -> PayloadStore {
        let dir = tempfile::tempdir().expect("临时目录");
        PayloadStore::new(komo_store::SessionPaths::at(dir.keep()))
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
                tool_calls: vec![ToolCallRequest {
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
                    tool_calls: vec![ToolCallRequest {
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
