//! 领到一个 Run 之后，这一段要的上下文从哪来（`SegmentSource` 的生产实现）。
//!
//! 三样东西在这里凑齐（`agent::handler` 的模块注释列的就是它们）：
//!
//! - **工作目录与已授权根**：Session 的 `workdir`，没有就是 `workspaces/`。
//! - **`TurnRequest`**：系统提示 + 回放窗口（最新一个 `conversation.boundary` 之后的
//!   消息，`Surface::replay`）+ 执行器挂着的工具 Schema。
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
use komo_kernel::events::{Event, EventPayload};
use komo_kernel::fold::{Surface, SurfaceMessage, fold};
use komo_kernel::projection::{ProjectionContext, ToolResultFacts, project};
use komo_kernel::traits::{ApprovalRepo, Ledger, LedgerError, ToolOutputStore};
use komo_kernel::types::delegate::DelegateSpec;
use komo_kernel::types::ids::{RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::plan::ExecutionPlan;
use komo_kernel::types::status::ToolCallState;
use komo_kernel::types::tool::{CancelToken, ToolDefinition, WorkspaceRoot};
use komo_kernel::types::turn::{ReplayMessage, ToolCallRequest, ToolResultForModel, TurnRequest};
use komo_runtime::agent::handler::SegmentSource;
use komo_runtime::agent::{Budget, ResumedRound, RetryBudget, Segment};
use komo_runtime::config::ConfigHolder;
use komo_runtime::executor::{CallEnv, CallRequest, resumed_from};
use komo_runtime::memory::MemoryManager;
use komo_runtime::scheduler::HandlerError;
use komo_runtime::tools::paths;
use komo_store::db::store_to_ledger;
use komo_store::{CheckpointStore, Db, PayloadStore, RecoveryStore};

use super::DELEGATE_TOOL;
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
    /// §5.6 的 skills 目录行。**启动时算一次**，之后只有配置重载会重算——它在一个
    /// 前缀里，每一段都重扫目录就成了"每次都不一样的前缀"。
    ///
    /// 与 Gateway 共享同一个 `Arc`：重载时换的是这里面的字符串，不必重建装配层。
    skills: Option<Arc<std::sync::RwLock<String>>>,
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
        }
    }

    /// 接上系统提示里的 skills 目录（§5.6）。
    pub fn with_skills(mut self, skills: Arc<std::sync::RwLock<String>>) -> Self {
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
        run_roots(self.session_files.as_deref(), session, cwd)
    }

    /// 现在这一份 skills 目录。没接就是空的——精简装配（测试、`skills` 关掉的部署）
    /// 不该因为少这一块就少别的。
    fn skills_prompt(&self) -> String {
        self.skills
            .as_ref()
            .map(|skills| skills.read().expect("skills 目录").clone())
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
        tools: Vec<ToolDefinition>,
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

        let cwd = session_record
            .as_ref()
            .and_then(|record| record.workdir.clone())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.workspaces.clone());
        // **根必须是真实路径**：工具解析目标时解掉符号链接（`tools::paths::resolve`），
        // 根停在字面上就会让 workspace 里的动作被判成"范围外"（macOS 的 `/tmp`、`/var`
        // 都是链接）。两边同一个口径，前缀匹配才是"在不在这个根里"。
        let model_result_bytes = self.model_result_bytes();
        let cwd = paths::real_root(&cwd);
        let roots = self.roots_for(&session, cwd.clone());
        // 投影要拿它把引用拼成能直接 `read` 的绝对路径。
        let session_root = self.session_files.as_ref().map(|dir| {
            paths::real_root(
                komo_store::SessionPaths::new(dir, &session)
                    .root()
                    .to_path_buf()
                    .as_path(),
            )
        });

        // 这是一条**子代理**吗？（§4）是的话，它的契约与预算跟着它走——父侧派它时给的那份，
        // 落在它自己的 `run.accepted` 里，所以重启之后也读得到。
        let delegate = surface
            .runs
            .get(&run)
            .and_then(|view| view.delegate.clone());

        // 子代理只拿得到任务本身：不注入记忆、不列 Skills、也**不带上父的对话历史**（回放
        // 窗口按本 Run 过滤，见下）。自包含这件事是父侧的责任，提示词里对它也说了。
        let memories = match &delegate {
            Some(_) => Vec::new(),
            None => self.recall_for(&session, &run, &surface).await,
        };

        let (prompt, tools) = match &delegate {
            Some(spec) => {
                let prompt = subagent_prompt(&cwd, &tools, spec);
                // 深度只有一层：`delegate` 不列给它。真正的拦在 runtime 的编排里（工具表
                // 是 UX，模型自己拼出这个名字不该能绕过不变量）。
                let offered = tools
                    .into_iter()
                    .filter(|tool| tool.name != DELEGATE_TOOL)
                    .collect();
                (prompt, offered)
            }
            None => (system_prompt(&cwd, &tools, &self.skills_prompt()), tools),
        };

        let request = TurnRequest {
            session: session.clone(),
            run: run.clone(),
            model: record.model.clone(),
            system_prompt: prompt,
            // **按本 Run 过滤**：子代理跑过的那几轮属于它自己那条 Run，父续跑时读回来的
            // 必须是父自己的上下文——否则子代理的探索过程会跑进父的窗口，而父侧本来只
            // 该拿到那条结果（§4）。
            messages: replay(
                &surface,
                &run,
                self.outputs.as_deref(),
                session_root.as_deref(),
                model_result_bytes,
            )
            .await,
            tools,
            memories,
            covers: None,
        };

        let env = CallEnv {
            session: session.clone(),
            run: run.clone(),
            source: record.source.clone(),
            cwd,
            roots,
            model_result_bytes,
            session_root: session_root.clone(),
            env_version: None,
            principal: None,
            // 本 Run 是被谁派的（普通 Run 是 None）。runtime 用它硬拦"子代理再委派"。
            delegated: delegate.clone(),
            model: record.model.clone(),
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
    async fn recall_for(
        &self,
        session: &SessionId,
        run: &RunId,
        surface: &Surface,
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
            .prepare_segment(session, run, &text, &carried, surface.boundary())
            .await
            .uses
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

/// 回放窗口：最新一个 `conversation.boundary` 之后的消息（§13.1 的 `/new`）。
///
/// **还没被领走的 Run 的用户消息不进窗口**（§8.4「同一 Session 后续 Run 不越过它」）。
/// 输入是接收那一刻就落盘的（§8.5 内容权威），所以它在日志里的位置**早于**当前这一轮
/// 的收尾；照搬位置发出去，provider 看到的就是"助手要了一次调用、紧接着另一个 Run 的
/// 用户消息、最后才是那次调用的输出"，直接 400（`No tool output found for tool call …`）。
/// 领取那一步已经保证后一个 Run 不会先跑（`DUE_SQL`），这里管的是它**还没跑**时那半句话。
///
/// 每条工具结果的正文走的是**同一个投影函数**（`komo_kernel::projection`）：刚跑完那次与
/// 重启之后回放，必须是同一段字节。所以这里要把事实凑齐——工具名从那一轮的调用里找，
/// 完整正文与流大小从 `output.json` 读回来；读不回来时退回账本里那 1 KiB（那是"我们至少
/// 还有这些"，不是"本该如此"）。
async fn replay(
    surface: &Surface,
    only: &RunId,
    outputs: Option<&dyn ToolOutputStore>,
    session_root: Option<&std::path::Path>,
    model_result_bytes: usize,
) -> Vec<ReplayMessage> {
    let mut out = Vec::new();
    for message in window(surface, Some(only)) {
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
            let facts = ToolResultFacts {
                tool: tool_name(surface, &result.call).unwrap_or("?"),
                status: result.status,
                elapsed_ms: result.elapsed_ms,
                text: text.as_deref(),
                output: &result.output,
                stdout: result.stdout.as_ref(),
                stderr: result.stderr.as_ref(),
                session_root,
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
            text: message.text.clone(),
            tool_calls: message.tool_calls.clone(),
            tool_results,
            provider_blocks: message.provider_blocks.clone(),
        });
    }
    out
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
fn window<'s>(surface: &'s Surface, only: Option<&RunId>) -> Vec<&'s SurfaceMessage> {
    let kept: Vec<&SurfaceMessage> = surface
        .replay()
        .iter()
        // `only` = 只要**本 Run 自己**的那几句。子代理与父在同一份日志里，但它们不是同一段
        // 对话：子代理跑过的那几轮不能进父的窗口，父的也没进过子代理的（§4）。
        .filter(|message| only.is_none_or(|run| message.run.as_ref() == Some(run)))
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
        .rfind(|message| message.role == komo_kernel::types::turn::Role::User)
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
) -> Vec<WorkspaceRoot> {
    let mut roots = vec![WorkspaceRoot {
        path: cwd,
        writable: true,
        label: "workspace".into(),
    }];
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
/// `skills` 是 [`crate::service::state::skills_prompt`] 在**启动时**算好的那一块
/// （§5.6：目录行是启动快照，为的是提示前缀稳定）；没有能露面的 skills 时它是空串。
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
    /// 读进去，而写进去不许命中"范围内写入"那条 Allow。
    #[test]
    fn a_run_can_read_its_own_session_output_but_not_write_it() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::from_raw("sess-1");
        let roots = run_roots(Some(dir.path()), &session, PathBuf::from("/tmp/w"));

        let labels: Vec<&str> = roots.iter().map(|root| root.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["workspace", "session-output", "session-artifacts"],
            "工作目录可写，Session 自己的两处只读"
        );
        assert!(roots[0].writable);

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

        // 没接 `sessions/` 的精简装配只有工作目录——不该凭空多出两个根。
        let bare = run_roots(None, &session, PathBuf::from("/tmp/w"));
        assert_eq!(bare.len(), 1);
    }
}
