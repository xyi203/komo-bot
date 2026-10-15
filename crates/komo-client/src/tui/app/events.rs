//! 服务端事件：状态机的输入另一侧（[`super::App`] 的方法）。

use komo_kernel::events::{Event, EventPayload};
use komo_kernel::protocol::http::InterventionSummary;
use komo_kernel::protocol::sse::{SseEvent, SseFrame};
use komo_kernel::types::ids::RunId;
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::status::{RunState, ToolCallState};

use super::{
    App, Draft, Effect, Phase, ServerEvent, SubmissionState, blank_tool, intervention_kind_text,
    resume_summary, status_summary, verdicts_text, wait_text,
};
use crate::tui::model_picker::ModelPicker;

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
                // §8.8：打开就问一次待处理清单。清单是**问来的**——待处理可以属于别的会话
                // （定时任务、聊天里那条），本会话的事件流里没有它们。
                vec![Effect::FetchPending]
            }
            ServerEvent::Frame(frame) => {
                self.cursor = self.cursor.max(frame.id);
                self.frame(*frame)
            }
            ServerEvent::FrameRetired { id } => {
                // 旧 Gateway 还会推的退役帧（审批那两种，2026-10-11 删除）：游标照走，不说话
                // ——它不是"两端格式对不上"，提示一句只会让人去查一个不存在的问题。
                self.cursor = self.cursor.max(id);
                Vec::new()
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
                    // 断过又连上：多半是 Gateway 重启了。重启时的恢复扫描可能把一条 Run 停在
                    // 等人上——那一笔只落在 state.db 里，事件流里没有它（§8.8：介入的当前状态
                    // 直接查）。不问一次清单，界面会一直以为那条 Run 还在跑。
                    crate::sse::ConnectionState::Connected if was_reconnecting => {
                        self.connection = state;
                        self.note("事件订阅已恢复");
                        return vec![Effect::FetchPending];
                    }
                    _ => {}
                }
                self.connection = state;
                Vec::new()
            }
            ServerEvent::Answered(response) => {
                self.pending.remove(&response.handle);
                // 那句话由服务端给（`note`）：同一个结论的措辞不该在四个界面里各写一遍。
                let mut text = response.note.clone();
                if response.already_answered {
                    text.push_str("（早已答复，这次没有改变什么）");
                }
                self.note(text);
                Vec::new()
            }
            ServerEvent::Resumed(response) => {
                // 「没有未完成的任务」是**默认情况**，不值得占一行：§8.8 要的是"2 个任务
                // 已接续，1 个结果不明"那种真有事的时候说得出话。
                if let Some(summary) = resume_summary(&response) {
                    self.note(summary);
                }
                // 问一次清单：状态行的条数与 `/pending` 都读它（§8.8）。
                vec![Effect::FetchPending]
            }
            ServerEvent::Status(detail) => {
                self.note(status_summary(&detail, self.pending_count()));
                Vec::new()
            }
            ServerEvent::Pending(items) => {
                self.replace_pending(items);
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
                if count > 0 {
                    // 清单是给人挑的，答法得跟着（§11.3：每一条都要写清该答什么；一次答一条）。
                    self.note("答一条：/answer <句柄> <结论>");
                }
                Vec::new()
            }
            ServerEvent::PendingRefreshed(items) => {
                // 定期再问回来的（[`super::App::poll_pending`]）：清单没变时每拍印一遍是刷屏，
                // 变了状态行的条数与提示行自己会跟着变——要看全文的人有 `/pending`。
                self.replace_pending(items);
                Vec::new()
            }
            ServerEvent::ModelMenu(models) => {
                self.model_menu = models;
                if std::mem::take(&mut self.listing_models) {
                    if self.model_menu.is_empty() {
                        // 没有清单就没有可挑的：说清楚，留 `/model <id>` 这条路。
                        self.note(self.model_blurb());
                        self.note(self.effort_blurb());
                    } else {
                        self.model_picker = Some(ModelPicker::new(
                            self.model_menu.clone(),
                            self.model.as_deref(),
                        ));
                    }
                }
                Vec::new()
            }
            ServerEvent::Failed(text) => {
                self.fail(text);
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
            SseEvent::RunStatus { run, state } => {
                // 「停在等人上」：权威清单在 `GET /v1/interventions`——顺手问一遍，于是状态行
                // 的待处理条数跟得上。
                //
                // 帧里没有 `WaitReason`（等时钟的 `retry` 与等人的那种共用一个 `waiting`），
                // 所以这里分不出是哪一种：多问一次清单比漏掉一条便宜（清单只是一次
                // 派生查询，而漏掉的那条会让界面停在"等待中"）。
                if state == RunState::Waiting {
                    return vec![Effect::FetchPending];
                }
                // 清单上挂着的那条动起来了（前提回来、自己接着跑了）：清单该换一份，否则状态
                // 行上会一直挂着一条已经不在的待处理。
                if self
                    .pending
                    .values()
                    .any(|item| item.run.as_ref() == Some(&run))
                {
                    return vec![Effect::FetchPending];
                }
                // 本会话的一条 Run 结束了，清单上还挂着别的：这一轮里多半有人（或模型在 shell
                // 里）答过其中一条，立刻悄悄问一次，不等下一拍轮询。
                if state.is_terminal() && !self.pending.is_empty() {
                    return vec![Effect::RefreshPending];
                }
                Vec::new()
            }
            SseEvent::Heartbeat => Vec::new(),
        }
    }

    /// 换上一份新问到的清单。
    ///
    /// **替换**，不是往里加：它就是 `GET /v1/interventions` 的当前答案，而"上一次问到的、
    /// 这次已经不在了"那些必须跟着消失——否则状态行会一直挂着一条早就答过的。
    fn replace_pending(&mut self, items: Vec<InterventionSummary>) {
        self.pending = items
            .into_iter()
            .map(|item| (item.handle.clone(), item))
            .collect();
    }

    /// 吃进一条 JSONL 事件：折进 Surface，再补两张 Surface 说不出的边表。
    fn absorb(&mut self, event: &Event) {
        // 补读与订阅各读各的一段（订阅从游标之后起），照理不会重叠；真重了也不能把用量
        // 加两遍——**只有没见过的那一条才计数**。游标不能当这个记号：它在别处（`frame`）
        // 就已经推过了，等走到这里永远是"见过"。
        let fresh = event.seq > self.counted;
        self.counted = self.counted.max(event.seq);
        self.surface.extend([event]);
        self.cursor = self.cursor.max(event.seq);

        if let Some(run) = &event.run {
            self.follow(run, &event.payload);
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
            | EventPayload::RunAbandoned(_)
            | EventPayload::RunAbsorbed(_) => {
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
                // 状态由 fold 折出来，读它的是状态行；这里只说**要人**的那一种——等干预
                // （结果不明 / 阻塞）正是 §7.5 清单的内容，而"排队等时钟"不是（它是状态行上的一句
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
                // 这一段对话到此为止花了多少 token：状态行上那一格读它。模型每答一轮报一
                // 次，累加起来就是这个会话的用量（补读历史时一并算上，所以 resume 回来看
                // 到的是整段的数，不是"这次打开之后"的数）。
                if fresh {
                    self.tokens.add(body);
                }
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
                // 顶层那份截在 1 KiB，完整正文在 body 里（SSE 原样推过来了）。
                line.preview = body
                    .body
                    .as_ref()
                    .and_then(|full| full.preview.clone().or_else(|| full.error.clone()))
                    .or_else(|| body.preview.clone());
                line.elapsed_ms = body.elapsed_ms;
            }
            _ => {}
        }
    }

    /// 状态行跟着哪条 Run（[`super::App::current_run`]）：通常是最近一条事件的那条，两种例外。
    ///
    /// - 正跑着一条时，**排队的那条不抢**：在跑的时候打一句话（或来一条监听事件），状态行
    ///   照旧说在跑的那条、Esc 照旧取消它；排队的那条开跑时它自己的事件会把状态行拿过去。
    /// - 被插话并走的那条（§6）：回答在宿主那里，跟宿主。
    fn follow(&mut self, run: &RunId, payload: &EventPayload) {
        if let EventPayload::RunAbsorbed(body) = payload {
            self.current_run = Some(body.into.clone());
            return;
        }
        let status = |run: &RunId| self.surface.runs.get(run).map(|view| view.status);
        let busy = self
            .current_run
            .as_ref()
            .filter(|current| *current != run)
            .and_then(status)
            .is_some_and(|state| !state.is_terminal() && !state.awaits_claim());
        if busy && status(run).is_some_and(RunState::awaits_claim) {
            return;
        }
        self.current_run = Some(run.clone());
    }

    /// `Ctrl-T`：工具调用全部展开 / 折叠。
    ///
    /// 已经落进终端回滚区的那些也要跟着变，而那几行终端不让改——所以整段重画（见
    /// [`super::App::repaint`]）。
    pub fn toggle_tool_detail(&mut self) {
        self.tool_detail = !self.tool_detail;
        self.repaint = true;
    }
}

/// 清单上的一条：句柄、种类、问题，以及**这一条此刻能答什么**。
///
/// §11.3 要求 `/pending` 的每一行「写清这一条该答什么」——句柄是 Run ID，结论就是清单自带的那一份（自己推会推出一个按下去没反应的答案）。
fn pending_line(item: &InterventionSummary) -> String {
    format!(
        "{}  {}  {}  可答：{}",
        item.handle,
        intervention_kind_text(item.kind),
        item.question,
        verdicts_text(&item.verdicts)
    )
}
