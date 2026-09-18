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
use komo_kernel::traits::{ApprovalRepo, Ledger, LedgerError};
use komo_kernel::types::ids::{RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::status::ToolCallState;
use komo_kernel::types::tool::{CancelToken, ToolDefinition, WorkspaceRoot};
use komo_kernel::types::turn::{ReplayMessage, ToolResultForModel, TurnRequest};
use komo_runtime::agent::handler::SegmentSource;
use komo_runtime::agent::{Budget, ResumedRound, RetryBudget, Segment};
use komo_runtime::executor::{CallEnv, CallRequest, resumed_from};
use komo_runtime::memory::MemoryManager;
use komo_runtime::scheduler::HandlerError;
use komo_runtime::tools::paths;
use komo_store::{CheckpointStore, Db, RecoveryStore};

use super::ledgers::RoutedLedger;

/// 装配执行段，并持有每个 Run 的取消开关。
pub struct GatewaySegments {
    routed: Arc<RoutedLedger>,
    db: Db,
    approvals: Arc<dyn ApprovalRepo>,
    /// 读不出来的会话要停在 `needs_attention` 上，而不是被反复领取——写那一笔要它。
    recovery: RecoveryStore,
    workspaces: PathBuf,
    max_rounds: u32,
    max_retries: u32,
    cancels: Mutex<BTreeMap<RunId, CancelToken>>,
    /// `None` = 这台 Gateway 没有记忆这一层（测试里的精简装配）。
    memories: Option<Arc<MemoryManager>>,
    checkpoints: Option<CheckpointStore>,
    /// Cron Job 的**执行预算**要从 Job 上读（§10：每个 Job 有自己的执行预算）。
    /// `None` = 没接 Cron 这一层，所有 Run 都用全局的 `max_rounds`。
    cron: Option<Arc<dyn komo_kernel::traits::CronRepo>>,
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
            max_rounds,
            max_retries,
            cancels: Mutex::new(BTreeMap::new()),
            cron: None,
            memories: None,
            checkpoints: None,
        }
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
        let cwd = paths::real_root(&cwd);
        let roots = vec![WorkspaceRoot {
            path: cwd.clone(),
            writable: true,
            label: "workspace".into(),
        }];

        let memories = self.recall_for(&session, &run, &surface).await;

        let request = TurnRequest {
            session: session.clone(),
            run: run.clone(),
            model: record.model.clone(),
            system_prompt: system_prompt(&cwd, &tools),
            messages: replay(&surface),
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
            env_version: None,
            principal: None,
            cancel: self.token_for(&run),
        };

        let budget = Budget {
            max_rounds: self.max_rounds_for(&record.source).await,
            max_tokens: None,
            first_round: rounds_so_far + 1,
            retry: RetryBudget {
                attempts: record.retry_attempts,
                max_attempts: self.max_retries,
                ..RetryBudget::default()
            },
        };

        let resume = self.resumed(&surface, &events, &run).await;

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
        memories.prepare(run, &text, &carried).await.uses
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
    async fn resumed(
        &self,
        surface: &Surface,
        events: &[Event],
        run: &RunId,
    ) -> Option<ResumedRound> {
        // 这个 Run 得在这个会话里。
        surface.runs.get(run)?;
        let waiting = waiting_approval(events, run);
        let mut pending = Vec::new();
        // **这一轮还没有结果的调用，全部交回执行器**——不只是已经写过计划的那几个：
        // 一轮里前一个停下时，后面的调用连 `tool.planned` 都还没有（`Surface::open_calls`）。
        for call_id in &surface.open_calls(run) {
            // 有那一行就用它的状态；没有（连计划都还没落盘）就是 §8.4 第 6 行说的
            // "确定尚未执行"——一次尝试都还没有过。
            let (state, attempt, attempts) = match surface.calls.get(call_id) {
                Some(call) => (call.state, call.attempt.clone(), call.attempts),
                None => (ToolCallState::Planned, None, 0),
            };
            let request = call_request(surface, events, call_id)?;
            let approval = waiting
                .as_ref()
                .filter(|(_, on)| on.as_ref() == Some(call_id))
                .map(|(approval, _)| approval.clone());
            pending.push(CallRequest {
                resumed: Some(resumed_from(state, attempt, attempts)),
                approval,
                ..request
            });
        }
        if pending.is_empty() {
            return None;
        }
        // 已经收尾的那些在回放窗口里（`Role::Tool` 的消息），不必再交一遍。
        let settled: Vec<ToolResultForModel> = Vec::new();
        // 停在审批上的调用，决定还没写下来就别再跑一遍——那会把同一个问题问第二次。
        if let Some((approval, _)) = &waiting
            && let Ok(Some(record)) = self.approvals.get(approval).await
            && record.decision.is_none()
        {
            tracing::debug!(run = %run, approval = %approval, "审批还没有结论，这一段不续跑");
            return None;
        }
        Some(ResumedRound { settled, pending })
    }
}

/// 这个 Run 停在哪条审批、哪个调用上。
fn waiting_approval(
    events: &[Event],
    run: &RunId,
) -> Option<(komo_kernel::types::ids::ApprovalId, Option<ToolCallId>)> {
    events
        .iter()
        .rev()
        .filter(|event| event.run.as_ref() == Some(run))
        .find_map(|event| match &event.payload {
            EventPayload::RunWaitingApproval(body) => {
                Some((body.approval.clone(), body.call.clone()))
            }
            _ => None,
        })
}

/// 从日志里把一个调用的原始请求与计划找回来。
fn call_request(surface: &Surface, events: &[Event], call: &ToolCallId) -> Option<CallRequest> {
    let requested = events.iter().rev().find_map(|event| match &event.payload {
        EventPayload::MessageAssistant(body) => body
            .tool_calls
            .iter()
            .find(|candidate| &candidate.call_id == call)
            .cloned(),
        _ => None,
    })?;
    let plan = events.iter().rev().find_map(|event| match &event.payload {
        EventPayload::ToolPlanned(body) if &body.call_id == call => {
            body.plan.as_ref().map(|plan| (**plan).clone())
        }
        _ => None,
    });
    let _ = surface;
    Some(CallRequest {
        call: call.clone(),
        provider_call_id: requested.provider_call_id,
        tool: requested.name,
        arguments: requested.arguments,
        plan,
        resumed: None,
        approval: None,
    })
}

/// 回放窗口：最新一个 `conversation.boundary` 之后的消息（§13.1 的 `/new`）。
///
/// **还没被领走的 Run 的用户消息不进窗口**（§8.4「同一 Session 后续 Run 不越过它」）。
/// 输入是接收那一刻就落盘的（§8.5 内容权威），所以它在日志里的位置**早于**当前这一轮
/// 的收尾；照搬位置发出去，provider 看到的就是"助手要了一次调用、紧接着另一个 Run 的
/// 用户消息、最后才是那次调用的输出"，直接 400（`No tool output found for tool call …`）。
/// 领取那一步已经保证后一个 Run 不会先跑（`DUE_SQL`），这里管的是它**还没跑**时那半句话。
fn replay(surface: &Surface) -> Vec<ReplayMessage> {
    window(surface)
        .into_iter()
        .map(|message| ReplayMessage {
            role: message.role,
            seq: message.seq,
            text: message.text.clone(),
            tool_calls: message.tool_calls.clone(),
            tool_results: message
                .tool_results
                .iter()
                .map(|result| ToolResultForModel {
                    provider_call_id: provider_call_id(surface, &result.call)
                        .unwrap_or_else(|| result.call.to_string()),
                    call_id: result.call.clone(),
                    content: result
                        .preview
                        .clone()
                        .unwrap_or_else(|| format!("[完整输出：{}]", result.output.path())),
                    is_error: !matches!(
                        result.status,
                        komo_kernel::types::refs::ToolResultStatus::Completed
                    ),
                })
                .collect(),
            provider_blocks: message.provider_blocks.clone(),
        })
        .collect()
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
fn window(surface: &Surface) -> Vec<&SurfaceMessage> {
    let kept: Vec<&SurfaceMessage> = surface
        .replay()
        .iter()
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
    window(surface)
        .into_iter()
        .rfind(|message| message.role == komo_kernel::types::turn::Role::User)
        .and_then(|message| message.text.clone())
}

/// 结果要按 provider 自己的 call_id 回传（§6）。
fn provider_call_id(surface: &Surface, call: &ToolCallId) -> Option<String> {
    surface.messages.iter().rev().find_map(|message| {
        message
            .tool_calls
            .iter()
            .find(|candidate| &candidate.call_id == call)
            .map(|candidate| candidate.provider_call_id.clone())
    })
}

/// W4 的最小系统提示。
fn system_prompt(cwd: &std::path::Path, tools: &[ToolDefinition]) -> String {
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    format!(
        "你是 komo，一个在用户自己机器上运行的助手。\n\
         工作目录：{}\n\
         可用工具：{}\n\
         危险操作会被拦下来等人批准；被拒绝就把它当作结果，不要绕过。\n\
         做完之后如实报告做了什么、有什么证据；没有证据就说没有。",
        cwd.display(),
        if names.is_empty() {
            "（这一段没有工具）".to_string()
        } else {
            names.join("、")
        }
    )
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

    #[test]
    fn a_call_request_comes_back_with_its_provider_id_and_arguments() {
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
        let surface = fold(&events);
        let request = call_request(&surface, &events, &call).expect("找得回来");
        assert_eq!(request.provider_call_id, "pc-7");
        assert_eq!(request.tool, "read");
        assert_eq!(request.arguments["path"], "a.txt");
        assert!(request.plan.is_none(), "还没有计划落盘");
    }

    #[test]
    fn the_waiting_approval_is_the_latest_one() {
        let run = RunId::from_raw("run-1");
        let events = vec![
            event(
                1,
                &run,
                EventPayload::RunWaitingApproval(komo_kernel::events::RunWaitingApproval {
                    approval: komo_kernel::types::ids::ApprovalId::from_raw("ap-1"),
                    call: Some(ToolCallId::from_raw("call-1")),
                }),
            ),
            event(
                2,
                &run,
                EventPayload::RunWaitingApproval(komo_kernel::events::RunWaitingApproval {
                    approval: komo_kernel::types::ids::ApprovalId::from_raw("ap-2"),
                    call: Some(ToolCallId::from_raw("call-2")),
                }),
            ),
        ];
        let (approval, call) = waiting_approval(&events, &run).expect("停在审批上");
        assert_eq!(approval.as_str(), "ap-2");
        assert_eq!(call, Some(ToolCallId::from_raw("call-2")));
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
        );
        assert!(prompt.contains("read"), "{prompt}");
        assert!(prompt.contains("/tmp/w"), "{prompt}");
    }
}
