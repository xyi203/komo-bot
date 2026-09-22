//! 服务端事件：状态机的输入另一侧（[`super::App`] 的方法）。

use komo_kernel::events::{Event, EventPayload};
use komo_kernel::protocol::http::{InterventionKind, InterventionSummary};
use komo_kernel::protocol::sse::{SseEvent, SseFrame};
use komo_kernel::types::ids::ApprovalId;
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::status::{RunState, ToolCallState};

use super::{
    App, Draft, Effect, Phase, ServerEvent, SubmissionState, blank_tool, intervention_kind_text,
    resume_summary, status_summary, verdicts_text, wait_text,
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
                // 补读了几条不用报：正在补读这件事状态行上说过了，读完了屏幕上就是那段
                // 历史本身——再补一句「补读了 33 条事件」，是让人读一个他已经看见的事实。
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
                // 清单才是权威（条数、`a` 的名单、`/pending` 都读它），这里只把弹窗支起来。
                // 详情是**因为清单里有它**才去取的，所以那一份摘要已经在 `pending` 里。
                if self
                    .approval
                    .as_ref()
                    .is_none_or(|open| open.record.approval != record.approval)
                {
                    self.approval = Some(ApprovalModal::new(*record));
                }
                Vec::new()
            }
            ServerEvent::Answered(response) => {
                self.pending.remove(&response.handle);
                // 弹窗上那一条答完了，它那份"我们自己答的"记号也可以收了；收不掉也不会错
                // （`approval_decided` 那一路还会再收一次），只是长会话里会攒。
                self.forget_answering_for(&response.handle);
                self.close_modal_if_gone();
                // 那句话由服务端给（`note`）：同一个结论的措辞不该在四个界面里各写一遍。
                let mut text = response.note.clone();
                if response.already_answered {
                    text.push_str("（早已答复，这次没有改变什么）");
                }
                self.note(text);
                self.open_next_pending()
            }
            ServerEvent::BatchAnswered(response) => {
                // 每一条都单独印：批里某一条可能早就答过了，而它**原来那个结论**说不定与
                // 这一批相反（服务端把原结论原样带回来）。只报个数就把那一条藏起来了。
                for answer in &response.answered {
                    self.pending.remove(&answer.handle);
                    self.forget_answering_for(&answer.handle);
                    let mut text = answer.note.clone();
                    if answer.already_answered {
                        text.push_str("（早已答复，这次没有改变什么）");
                    }
                    self.note(text);
                }
                for missing in &response.missing {
                    self.fail(format!("{missing} 没有这条待处理事项"));
                }
                self.close_modal_if_gone();
                self.open_next_pending()
            }
            ServerEvent::Resumed(response) => {
                // 「没有未完成的任务」是**默认情况**，不值得占一行：§8.8 要的是"2 个任务
                // 已接续，1 个等待审批"那种真有事的时候说得出话。
                if let Some(summary) = resume_summary(&response) {
                    self.note(summary);
                }
                // 「1 个等待审批」——问一次清单，有的话接着就弹（§8.8）。
                vec![Effect::FetchPending]
            }
            ServerEvent::Status(detail) => {
                self.note(status_summary(&detail, self.pending_count()));
                Vec::new()
            }
            ServerEvent::Pending(items) => {
                // 这一份**替换**清单，不是往里加：它就是 `GET /v1/interventions` 的当前
                // 答案，而"上一次问到的、这次已经不在了"那些必须跟着消失——否则状态行会一直
                // 挂着一条早就答过的。
                self.pending = items
                    .into_iter()
                    .map(|item| (item.handle.clone(), item))
                    .collect();
                // 先攒成文字再 `note`：`note` 要 `&mut self`，而这里还借着 `self.pending`。
                let lines: Vec<String> = self.pending.values().map(pending_line).collect();
                let count = lines.len();
                // **空清单只在被问到时才说**：启动、重连、每次答复之后都会问一次清单，
                // 每次都印一句"没有待处理的事项"是噪音；而 `/pending` 问了一句就必须有
                // 回答——那是它唯一的作用。
                if lines.is_empty() && std::mem::take(&mut self.asking_pending) {
                    self.note("没有待处理的事项");
                }
                for line in lines {
                    self.note(line);
                }
                if count > 1 {
                    // 清单是给人挑的，答案得跟着（§11.3：每一条都要写清该答什么）。
                    self.note(
                        "答一条：/approve <短ID>（审批）或 /answer <句柄> <结论>；\
                         多条审批一起答：/approve all",
                    );
                }
                // 清单里有、屏幕上没有的**审批**，就弹第一条（§8.8「打开即弹」）。
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

    /// 清单里还有没弹出来的**审批**，就取它第一条。
    ///
    /// 只挑审批（§7.5 的第一类）：弹窗是审批的主界面，而 `verify` / `blocked` 没有弹窗——
    /// 它们的出路在状态行、提示行与 `/pending` 的每一行里。三类共用一张清单，不等于共用
    /// 一种界面。
    ///
    /// 只在**没有弹窗**时动手：正在看的那一条不能被后面来的替换掉（§11.3 的"先把眼前
    /// 这件事答了"）。弹窗上的那一条已经在清单里，所以它不会把自己再取一次——除非详情
    /// 取失败（那之后 `approval` 是 `None`，重取一次是对的）。
    fn open_next_pending(&mut self) -> Vec<Effect> {
        if self.approval.is_some() {
            return Vec::new();
        }
        match self
            .pending
            .values()
            .find(|item| item.kind == InterventionKind::Approval)
        {
            Some(item) => vec![Effect::FetchApproval(item.handle.clone())],
            None => Vec::new(),
        }
    }

    /// 弹窗上那一条已经不在清单里了（自己答的、别处答的、或者服务端说它没了）就收窗。
    fn close_modal_if_gone(&mut self) {
        if self
            .approval
            .as_ref()
            .is_some_and(|open| !self.pending.contains_key(open.record.short_id.as_str()))
        {
            self.approval = None;
        }
    }

    /// 刚刚答复掉的那个句柄如果正是弹窗上这一条，把它在 [`App::answering`] 里的记号收掉。
    fn forget_answering_for(&mut self, handle: &str) {
        let answerable = self
            .approval
            .as_ref()
            .filter(|open| open.record.short_id.as_str() == handle)
            .map(|open| open.record.approval.clone());
        if let Some(approval) = answerable {
            self.answering.remove(&approval);
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
            SseEvent::RunStatus { run: _, state } => {
                // 「停在等人上」这个信号来得比审计补写早：`run.waiting` 走 Run 自己的
                // 路径，`approval.requested` 是随后补写的审计副本（§8.5 的反向顺序）。
                // 弹窗要的是后者，而权威清单在 `GET /v1/interventions`——顺手问一遍，
                // 于是弹窗不取决于那条审计副本什么时候落盘。
                //
                // 帧里没有 `WaitReason`（等时钟的 `retry` 与等人的两种共用一个 `waiting`），
                // 所以这里分不出是哪一种：多问一次清单比漏掉一条便宜（清单只是一次
                // 派生查询，而漏掉的那条会让界面停在"等待中"）。
                if state == RunState::Waiting {
                    return vec![Effect::FetchPending];
                }
                Vec::new()
            }
            SseEvent::ApprovalPending { .. } => {
                // **不靠这条通知传授权**（§13.1）：详情走
                // `GET /v1/interventions/{handle}`，由清单那一份带着句柄去取。
                // 这里只问一次清单：计数、`a` 的名单、`/pending` 都读它，而待处理可以
                // 属于**别的会话**——本会话的事件流里没有那几条。
                vec![Effect::FetchPending]
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
    ///
    /// **帧里只有审批 id、没有句柄**（§7.5 的句柄是短 ID），所以只知道"有一条答掉了"时
    /// 没法直接把它从清单里摘掉：那一条如果不是屏幕上这条，就问一次清单，让权威那一份
    /// 说话——自己猜哪一条没了，猜错的结果是状态行的数字一直不对。
    fn settle_remote(&mut self, approval: ApprovalId, approved: bool) -> Vec<Effect> {
        let mine = self.answering.remove(&approval);
        let open = self
            .approval
            .as_ref()
            .filter(|modal| modal.record.approval == approval)
            .map(|modal| modal.record.short_id.to_string());
        let Some(handle) = open else {
            return vec![Effect::FetchPending];
        };
        if !mine {
            let verdict = if approved { "已批准" } else { "已拒绝" };
            self.note(format!("这条审批在别处{verdict}了"));
        }
        // 有结论了，它就不该还挂在"待处理"那一份清单上。
        self.pending.remove(&handle);
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
            EventPayload::RunCompleted(_)
            | EventPayload::RunCancelled(_)
            | EventPayload::RunAbandoned(_) => {
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
            EventPayload::RunWaiting(body) => {
                // 状态由 fold 折出来，读它的是状态行；这里只说**要人**的那两种——等审批与
                // 等干预正是 §7.5 清单的内容，而"排队等时钟"不是（它是状态行上的一句
                // `retry`，说了只是噪音）。
                if body.reason.needs_a_person() {
                    let where_ = match &event.run {
                        Some(run) => format!("任务 {run}"),
                        None => "有一条任务".into(),
                    };
                    self.note(format!(
                        "{where_}停着等人：{}",
                        wait_text(&body.reason, self.now)
                    ));
                }
            }
            EventPayload::RunReclaimed(body) => {
                // 上一次执行没有收尾（§8.9）。它**不是一个状态**了，所以界面上唯一还能看见
                // 这件事的地方就是这里——不说就等于悄悄消失。
                let where_ = match &event.run {
                    Some(run) => format!("任务 {run}"),
                    None => "有一条任务".into(),
                };
                let reason = if body.reason.trim().is_empty() {
                    "上一个执行实例没有收尾".to_string()
                } else {
                    body.reason.clone()
                };
                self.note(format!("{where_}的领取权已交还（{reason}）"));
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

    /// `Ctrl-T`：工具调用印不印参数与结果预览。
    ///
    /// **它管的是之后印出来的那些**——已经落进终端回滚区的那几行是终端的了，这个开关
    /// 够不着它们（见 [`super::App::tool_detail`]）。
    pub fn toggle_tool_detail(&mut self) {
        self.tool_detail = !self.tool_detail;
    }
}

/// 清单上的一条：句柄、种类、问题，以及**这一条此刻能答什么**。
///
/// §11.3 要求 `/pending` 的每一行「写清这一条该答什么」——句柄是审批的短 ID 或另两类的
/// Run ID，结论就是清单自带的那一份（自己推会推出一个按下去没反应的答案）。
fn pending_line(item: &InterventionSummary) -> String {
    format!(
        "{}  {}  {}  可答：{}",
        item.handle,
        intervention_kind_text(item.kind),
        item.question,
        verdicts_text(&item.verdicts)
    )
}
