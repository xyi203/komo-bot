//! 服务端事件：状态机的输入另一侧（[`super::App`] 的方法）。

use komo_kernel::events::{Event, EventPayload};
use komo_kernel::protocol::sse::{SseEvent, SseFrame};
use komo_kernel::types::ids::{ApprovalId, ToolCallId};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::status::ToolCallState;

use super::{
    App, Draft, Effect, Phase, ServerEvent, SubmissionState, blank_tool, resume_summary,
    status_summary,
};
use crate::tui::approval::ApprovalModal;

impl App {
    // ---- 服务端事件 ----

    pub fn apply(&mut self, event: ServerEvent) -> Vec<Effect> {
        match event {
            ServerEvent::HistoryPage(page) => {
                for event in &page.events {
                    self.absorb(event);
                }
                let count = page.events.len();
                self.phase = match &self.phase {
                    Phase::Backfilling { events } => Phase::Backfilling {
                        events: events + count,
                    },
                    Phase::Interactive => Phase::Interactive,
                };
                self.cursor = self.cursor.max(page.next);
                Vec::new()
            }
            ServerEvent::HistoryDone => {
                if let Phase::Backfilling { events } = self.phase {
                    self.note(format!("补读了 {events} 条事件"));
                }
                self.phase = Phase::Interactive;
                // §8.8：等待审批的 Run 打开即弹。清单是**问来的**——待处理审批可以属于
                // 别的会话（定时任务、聊天里那条），本会话的事件流里没有它们。
                vec![Effect::FetchPending]
            }
            ServerEvent::Frame(frame) => {
                self.cursor = self.cursor.max(frame.id);
                self.frame(*frame)
            }
            ServerEvent::FrameSkipped { id, reason } => {
                self.cursor = self.cursor.max(id);
                tracing::debug!(seq = id.0, %reason, "跳过一帧读不懂的事件");
                // 一帧都解不开通常不是"未来的事件类型"，而是**两端说的不是同一种格式**——
                // 那时候每一帧都会走这里。静默跳过等于什么都不说：界面永远停在旧状态，
                // 看上去像服务端没干活。所以第一条要说出来，之后只在日志里。
                if !self.skipped_noticed {
                    self.skipped_noticed = true;
                    self.note(format!(
                        "seq {id} 这一帧读不懂，已跳过（{reason}）；后续同类只记日志"
                    ));
                }
                Vec::new()
            }
            ServerEvent::Connection(state) => {
                let was_reconnecting = matches!(
                    &self.connection,
                    crate::sse::ConnectionState::Reconnecting { .. }
                );
                match &state {
                    crate::sse::ConnectionState::Reconnecting { reason, .. }
                        if !was_reconnecting =>
                    {
                        self.fail(format!("事件订阅断开，正在重连：{reason}"));
                    }
                    crate::sse::ConnectionState::Closed { reason } => {
                        self.fail(format!("事件订阅已关闭：{reason}"));
                    }
                    _ => {}
                }
                self.connection = state;
                Vec::new()
            }
            ServerEvent::Approval(record) => {
                // 已经决定过的就不弹了——弹一个点了没用的窗是在骗人。
                if record.decision.is_some() {
                    return Vec::new();
                }
                self.pending
                    .insert(record.approval.clone(), (*record).clone());
                if self
                    .approval
                    .as_ref()
                    .is_none_or(|open| open.record.approval != record.approval)
                {
                    self.approval = Some(ApprovalModal::new(*record));
                }
                Vec::new()
            }
            ServerEvent::ApprovalSettled {
                approval,
                approved,
                already_decided,
            } => {
                let short = self
                    .pending
                    .remove(&approval)
                    .map(|record| record.short_id.to_string())
                    .unwrap_or_else(|| approval.to_string());
                // 自己这条的回执到了，`answering` 里的那一格可以收了（收不掉也不会错，
                // 只是长会话里会攒）。
                self.answering.remove(&approval);
                self.close_modal_if_gone();
                let verdict = if approved { "已批准" } else { "已拒绝" };
                if already_decided {
                    self.note(format!("{short} 早已决定（{verdict}），这次没有改变什么"));
                } else {
                    self.note(format!("{short} {verdict}"));
                }
                self.open_next_pending()
            }
            ServerEvent::BatchSettled(response) => {
                // 每一条都单独印：批里某一条可能早就决定过了，而它**原来那个决定**说不定
                // 与这一批相反（网关把原决定原样带回来）。只报个数就把那一条藏起来了。
                for decision in &response.decisions {
                    self.pending.remove(&decision.approval);
                    self.answering.remove(&decision.approval);
                    let verdict = if decision.decision.approved {
                        "已批准"
                    } else {
                        "已拒绝"
                    };
                    self.note(format!(
                        "{} {verdict}{}",
                        decision.short_id,
                        if decision.already_decided {
                            "（早已决定，这次没有改变什么）"
                        } else {
                            ""
                        }
                    ));
                }
                for missing in &response.missing {
                    self.fail(format!("{missing} 没有这条审批"));
                }
                self.close_modal_if_gone();
                self.open_next_pending()
            }
            ServerEvent::Resumed(response) => {
                self.note(resume_summary(&response));
                // 「1 个等待审批」——问一次清单，有的话接着就弹（§8.8）。
                vec![Effect::FetchPending]
            }
            ServerEvent::Status(detail) => {
                self.note(status_summary(&detail, self.pending_count()));
                Vec::new()
            }
            ServerEvent::Pending(records) => {
                // 这一份**替换**清单，不是往里加：它就是 `GET /v1/approvals` 的当前答案，
                // 而"上一次问到的、这次已经不在了"那些必须跟着消失——否则状态行会一直
                // 挂着一条早就答过的。
                self.pending = records
                    .into_iter()
                    .map(|record| (record.approval.clone(), record))
                    .collect();
                // 先攒成文字再 `note`：`note` 要 `&mut self`，而这里还借着 `self.pending`。
                let lines: Vec<String> = self
                    .pending
                    .values()
                    .map(|record| {
                        format!(
                            "{}  {}  {}",
                            record.short_id, record.plan.tool, record.reason
                        )
                    })
                    .collect();
                let count = lines.len();
                // **空清单只在被问到时才说**：启动、重连、每次决定之后都会问一次清单，
                // 每次都印一句"没有待处理的审批"是噪音；而 `/pending` 问了一句就必须有
                // 回答——那是它唯一的作用。
                if lines.is_empty() && std::mem::take(&mut self.asking_pending) {
                    self.note("没有待处理的审批");
                }
                for line in lines {
                    self.note(line);
                }
                if count > 1 {
                    // 清单是给人挑的，答案得跟着：**两条以上时**才提"全批"，一条时那句
                    // 话是噪音。
                    self.note(format!(
                        "答一条：/approve <短ID>；{count} 条全答：/approve all"
                    ));
                }
                // 清单里有、屏幕上没有的，就弹第一条（§8.8「打开即弹」）。
                self.open_next_pending()
            }
            ServerEvent::ModelMenu(models) => {
                self.model_menu = models;
                if std::mem::take(&mut self.listing_models) {
                    self.note(self.model_blurb());
                    self.note(self.effort_blurb());
                }
                Vec::new()
            }
            ServerEvent::Failed(text) => {
                self.fail(text);
                if let Some(modal) = self.approval.as_mut() {
                    // 答复没送到，允许再答一次。
                    modal.answering = false;
                }
                Vec::new()
            }
            ServerEvent::Submitted {
                request_key,
                response,
            } => {
                if let Some(pending) = self
                    .pending_submissions
                    .iter_mut()
                    .find(|pending| pending.request_key == request_key)
                {
                    pending.state = SubmissionState::Submitted {
                        run: response.run.clone(),
                        deduplicated: response.deduplicated,
                    };
                }
                Vec::new()
            }
            ServerEvent::SubmitFailed { request_key, error } => {
                if let Some(pending) = self
                    .pending_submissions
                    .iter_mut()
                    .find(|pending| pending.request_key == request_key)
                {
                    pending.state = SubmissionState::Failed {
                        error: error.clone(),
                    };
                } else {
                    self.fail(format!("提交失败：{error}"));
                }
                Vec::new()
            }
            ServerEvent::Notice(text) => {
                self.note(text);
                Vec::new()
            }
            ServerEvent::Tick(now) => {
                self.now = Some(now);
                Vec::new()
            }
        }
    }

    /// 清单里还有没弹出来的，就取它第一条。
    ///
    /// 只在**没有弹窗**时动手：正在看的那一条不能被后面来的替换掉（§11.3 的"先把眼前
    /// 这件事答了"）。弹窗上的那一条已经在清单里，所以它不会把自己再取一次——除非详情
    /// 取失败（那之后 `approval` 是 `None`，重取一次是对的）。
    fn open_next_pending(&mut self) -> Vec<Effect> {
        if self.approval.is_some() {
            return Vec::new();
        }
        match self.pending.keys().next() {
            Some(approval) => vec![Effect::FetchApproval(approval.clone())],
            None => Vec::new(),
        }
    }

    /// 弹窗上那一条已经不在清单里了（自己答的、别处答的、或者服务端说它没了）就收窗。
    fn close_modal_if_gone(&mut self) {
        if self
            .approval
            .as_ref()
            .is_some_and(|open| !self.pending.contains_key(&open.record.approval))
        {
            self.approval = None;
        }
    }

    fn frame(&mut self, frame: SseFrame) -> Vec<Effect> {
        match frame.event {
            SseEvent::Event(event) => {
                self.absorb(&event);
                Vec::new()
            }
            // 模型正在打字。**纯界面提示**：不进 `Surface`，丢了也不影响任何东西——
            // 一轮结束时 `message.assistant` 会把完整回复正式送到（kernel 的
            // `SseEvent::AssistantDelta` 注释）。
            SseEvent::AssistantDelta { run, round, text } => {
                self.current_run = Some(run.clone());
                match self.draft.as_mut() {
                    // 同一轮：接着打。
                    Some(draft) if draft.run == run && draft.round == round => {
                        draft.text.push_str(&text)
                    }
                    // 换了一轮或换了一个 Run：上一段草稿已经被它的 `message.assistant`
                    // 取代过了，这里重新起一段。
                    _ => self.draft = Some(Draft { run, round, text }),
                }
                Vec::new()
            }
            // Run 状态是派生的，我们自己折；收到它只当一次提示。
            SseEvent::RunStatus { run, status } => {
                if status.is_terminal() && self.current_run.as_ref() == Some(&run) {
                    self.scroll = 0;
                }
                Vec::new()
            }
            SseEvent::ApprovalPending { approval, .. } => {
                // **不靠这条通知传授权**（§13.1）：详情去 GET /v1/approvals/{id} 取。
                // 清单也顺手问一遍：计数、`a` 的名单、`/pending` 都读它，而待处理可以
                // 属于**别的会话**——本会话的事件流里没有那几条。
                vec![Effect::FetchApproval(approval), Effect::FetchPending]
            }
            SseEvent::ApprovalDecided { approval, approved } => {
                self.settle_remote(approval, approved)
            }
            SseEvent::Heartbeat => Vec::new(),
        }
    }

    /// 某个审批有结论了（SSE 的 `approval_decided`）。
    ///
    /// 它**分不出是谁答的**——这个客户端自己按下的那一下也走这条路。所以分：自己正在
    /// 等的那些（[`App::answering`]）安静地把窗关掉，别人的则说出来。
    fn settle_remote(&mut self, approval: ApprovalId, approved: bool) -> Vec<Effect> {
        let mine = self.answering.remove(&approval);
        let shown = self
            .approval
            .as_ref()
            .is_some_and(|open| open.record.approval == approval);
        if !mine && shown {
            let verdict = if approved { "已批准" } else { "已拒绝" };
            self.note(format!("这条审批在别处{verdict}了"));
        }
        self.pending.remove(&approval);
        self.close_modal_if_gone();
        self.open_next_pending()
    }

    /// 吃进一条 JSONL 事件：折进 Surface，再补两张 Surface 说不出的边表。
    fn absorb(&mut self, event: &Event) {
        self.surface.extend([event]);
        self.cursor = self.cursor.max(event.seq);

        if let Some(run) = &event.run {
            self.current_run = Some(run.clone());
        }
        match &event.payload {
            EventPayload::RunAccepted(body) => {
                self.pending_submissions
                    .retain(|pending| pending.request_key != body.request_key);
                if let Some(run) = &event.run {
                    let meta = self.runs.entry(run.clone()).or_default();
                    meta.started_at = Some(event.ts);
                    meta.model = body.model.clone();
                    meta.effort = body.effort.clone();
                }
            }
            EventPayload::RunCompleted(_) | EventPayload::RunCancelled(_) => {
                if let Some(run) = &event.run {
                    self.runs.entry(run.clone()).or_default().ended_at = Some(event.ts);
                    // Run 结束了就没人在打字了；留着一段孤儿草稿会一直显示"生成中"。
                    if self.draft.as_ref().is_some_and(|draft| &draft.run == run) {
                        self.draft = None;
                    }
                }
            }
            EventPayload::RunFailed(body) => {
                if let Some(run) = &event.run {
                    self.runs.entry(run.clone()).or_default().ended_at = Some(event.ts);
                    if self.draft.as_ref().is_some_and(|draft| &draft.run == run) {
                        self.draft = None;
                    }
                    self.fail(format!("任务 {run} 失败：{}", body.reason));
                } else {
                    self.fail(format!("任务失败：{}", body.reason));
                }
            }
            EventPayload::RunNeedsAttention(body) => {
                if let Some(run) = &event.run {
                    self.fail(format!("任务 {run} 需要处理：{}", body.reason));
                } else {
                    self.fail(format!("任务需要处理：{}", body.reason));
                }
            }
            EventPayload::ConfigChanged(body) => {
                if let Some(run) = &event.run {
                    let meta = self.runs.entry(run.clone()).or_default();
                    if body.model.is_some() {
                        meta.model = body.model.clone();
                    }
                    if body.effort.is_some() {
                        meta.effort = body.effort.clone();
                    }
                }
            }
            EventPayload::MessageAssistant(body) => {
                // 正式回复到了，草稿让位——消息面上这一轮只留一条。
                if self
                    .draft
                    .as_ref()
                    .is_some_and(|draft| Some(&draft.run) == event.run.as_ref())
                {
                    self.draft = None;
                }
                for call in &body.tool_calls {
                    let line = self.tools.entry(call.call_id.clone()).or_insert_with(|| {
                        blank_tool(call.call_id.clone(), event.run.clone(), event.seq)
                    });
                    line.tool = call.name.clone();
                    line.args = call.arguments.clone();
                }
            }
            EventPayload::ToolPlanned(body) => {
                let line = self.tools.entry(body.call_id.clone()).or_insert_with(|| {
                    blank_tool(body.call_id.clone(), event.run.clone(), event.seq)
                });
                if let Some(plan) = &body.plan {
                    line.tool = plan.tool.clone();
                    line.args = plan.args.clone();
                    line.cwd = plan.cwd.as_ref().map(|p| p.display().to_string());
                }
                line.state = ToolCallState::Planned;
            }
            EventPayload::ToolStarted(body) => {
                let line = self.tools.entry(body.call_id.clone()).or_insert_with(|| {
                    blank_tool(body.call_id.clone(), event.run.clone(), event.seq)
                });
                line.state = ToolCallState::Started;
                line.attempts += 1;
            }
            EventPayload::ToolResult(body) => {
                let line = self.tools.entry(body.call_id.clone()).or_insert_with(|| {
                    blank_tool(body.call_id.clone(), event.run.clone(), event.seq)
                });
                line.state = match body.status {
                    ToolResultStatus::Completed => ToolCallState::Completed,
                    ToolResultStatus::Failed => ToolCallState::Failed,
                    ToolResultStatus::Uncertain => ToolCallState::Uncertain,
                };
                line.preview = body.preview.clone();
                line.elapsed_ms = body.elapsed_ms;
            }
            _ => {}
        }
    }

    /// 展开 / 收起一次调用的完整参数与结果预览。
    pub fn toggle_tool(&mut self, call: &ToolCallId) {
        if let Some(line) = self.tools.get_mut(call) {
            line.expanded = !line.expanded;
        }
    }

    /// Ctrl-T：一次全展开或全收起。还有收着的就全展开，否则全收起。
    pub fn toggle_all_tools(&mut self) {
        let expand = self.tools.values().any(|line| !line.expanded);
        for line in self.tools.values_mut() {
            line.expanded = expand;
        }
    }
}
