//! 服务端事件：状态机的输入另一侧（[`super::App`] 的方法）。

use komo_kernel::events::{Event, EventPayload};
use komo_kernel::protocol::http::PendingItem;
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
                // §8.8：等待审批的 Run 打开即弹。
                self.fetch_first_pending()
            }
            ServerEvent::Frame(frame) => {
                self.cursor = self.cursor.max(frame.id);
                self.frame(*frame)
            }
            ServerEvent::FrameSkipped { id, reason } => {
                self.cursor = self.cursor.max(id);
                tracing::debug!(seq = id.0, %reason, "跳过一帧读不懂的事件");
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
                self.pending_short_ids
                    .insert(record.approval.clone(), record.short_id.clone());
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
                if self
                    .approval
                    .as_ref()
                    .is_some_and(|open| open.record.approval == approval)
                {
                    self.approval = None;
                }
                let short = self
                    .pending_short_ids
                    .remove(&approval)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| approval.to_string());
                let verdict = if approved { "已批准" } else { "已拒绝" };
                if already_decided {
                    self.note(format!("{short} 早已决定（{verdict}），这次没有改变什么"));
                } else {
                    self.note(format!("{short} {verdict}"));
                }
                self.fetch_first_pending()
            }
            ServerEvent::Resumed(response) => {
                self.note(resume_summary(&response));
                let mut effects = Vec::new();
                for item in &response.pending {
                    if let PendingItem::Approval(record) = item {
                        effects.push(Effect::FetchApproval(record.approval.clone()));
                        break;
                    }
                }
                effects
            }
            ServerEvent::Status(detail) => {
                self.note(status_summary(&detail));
                Vec::new()
            }
            ServerEvent::Pending(records) => {
                if records.is_empty() {
                    self.note("没有待处理的审批");
                }
                for record in &records {
                    self.pending_short_ids
                        .insert(record.approval.clone(), record.short_id.clone());
                    self.note(format!(
                        "{}  {}  {}",
                        record.short_id, record.plan.tool, record.reason
                    ));
                }
                Vec::new()
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

    fn fetch_first_pending(&self) -> Vec<Effect> {
        if self.approval.is_some() {
            return Vec::new();
        }
        match self.surface.pending_approvals.values().next() {
            Some(view) => vec![Effect::FetchApproval(view.approval.clone())],
            None => Vec::new(),
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
            SseEvent::ApprovalPending { approval, short_id } => {
                self.pending_short_ids.insert(approval.clone(), short_id);
                // **不靠这条通知传授权**（§13.1）：详情去 GET /v1/approvals/{id} 取。
                vec![Effect::FetchApproval(approval)]
            }
            SseEvent::ApprovalDecided { approval, approved } => {
                self.settle_remote(approval, approved)
            }
            SseEvent::Heartbeat => Vec::new(),
        }
    }

    /// 别处（聊天渠道）答了这条审批。
    fn settle_remote(&mut self, approval: ApprovalId, approved: bool) -> Vec<Effect> {
        if self
            .approval
            .as_ref()
            .is_some_and(|open| open.record.approval == approval)
        {
            self.approval = None;
            let verdict = if approved { "已批准" } else { "已拒绝" };
            self.note(format!("这条审批在别处{verdict}了"));
        }
        self.pending_short_ids.remove(&approval);
        self.fetch_first_pending()
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
