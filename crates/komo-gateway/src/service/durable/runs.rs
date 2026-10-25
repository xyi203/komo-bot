//! run 的视图：`RunSummary` / `RunDetail` / `RunState` 映射与取消（`docs/storage-durable-d5.md` §1 / §4 W2a）。
//!
//! 新存储里没有"run 表"。**`RunId` = 这条 run 首个输入 submission 的十进制 ID**，一条 run 的一切由
//! submission、entry 与 `komo.live` 拼出来：
//!
//! - 事实来源是 W1a 的投影（`legacy_events`）：一个 conversation 的 entry 与 submission 折成旧事件，
//!   再交给旧的 `fold`——工具调用、最终回复、轮数、失败原因、用量都从那一份读，和 SSE 补读同一个函数，
//!   所以 run 详情与事件流说的是同一件事。
//! - **状态**：`fold` 给 `Queued` / `Running` / `Completed` / `Failed` / `Cancelled` / `Absorbed`；
//!   当前 run 正在等重试时（`komo.live.generation.retry`）把 `Running` 改成 `Waiting + Retry`
//!   （`wait` 只会是 `Retry`，§1）。
//! - **时间**：`created_at` = 输入 entry 放置的时刻（排着队的 run 取受理时能看到的最近一条记录的时刻），
//!   `ended_at` = 终态帧的时刻。投影的帧时间取自记录里第一条消息的毫秒时间戳，所以 `ended_at` 是
//!   "最后一条消息的时刻"，不是 `komo.settled` 落下的那一刻。
//! - **`!` 命令**（`shell.command` write）也有 RunId（write submission 的 ID），详情由它的 entry 拼
//!   （见 [`super::shell`]）。
//! - `memories` / `recall`：读 `komo.memory` document（W3c）。
//!
//! 复制出来的会话里 `komo.settled` 的 `data.submission` 指向没复制的 submission：投影会为它出终态帧，
//! 这里**只认有 submission 记录的 run**（列表与详情都按 submission 找），所以那些不算 run。
//!
//! 整个 conversation 的投影是 O(历史) 的：`GET /v1/runs/{id}` 与会话详情的 `unfinished` 每次现算。
//! TODO：长会话上给 run 游标（只投影这条 run 之后的记录）。

use komo_kernel::durable::legacy_events::legacy_events;
use komo_kernel::durable::storage::{ConversationQuery, EntryQuery, SubmissionQuery};
use komo_kernel::durable::{
    ConversationId, ConversationRecord, Cursor, EntryId, EntryRecord, InputState, Message,
    ScanOrder, Storage, SubmissionId, SubmissionKind, SubmissionRecord, WriteState, legacy_ids,
};
use komo_kernel::events::{ContextCompacted, Event, EventPayload};
use komo_kernel::fold::{RunView, Surface, fold};
use komo_kernel::protocol::http::{
    CompactionSummary, RunDetail, RunSummary, TokenTotals, ToolCallSummary,
};
use komo_kernel::types::ids::{CronJobId, RunId, SessionId};
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::status::{RetryCause, RunState, WaitReason};
use komo_runtime::durable::harness::{GenerationLive, Harness, WithdrawResult};
use serde_json::Value;
use time::OffsetDateTime;

use super::DurableCore;
use super::sessions::{SessionError, time_of};
use super::shell::SHELL_ENTRY;

const SCAN_PAGE: usize = 512;

/// 一个 `Message` 若是 user 消息，它的文字（块里的文本拼起来）。
pub(super) fn user_text_of(message: &Message) -> Option<String> {
    match message {
        Message::User(user) => Some(super::tree::content_text(&user.content)),
        _ => None,
    }
}

/// 等重试的那一格：重试到点时刻、已用次数（`RunState::Waiting` 的 `wait`）。
pub(super) fn retry_wait(generation: &GenerationLive) -> Option<WaitReason> {
    let retry = generation.retry.as_ref()?;
    Some(WaitReason::Retry {
        attempts: generation.attempt,
        not_before: time_of(retry.at),
        cause: RetryCause::Transport,
    })
}

/// `submission` 此刻的 run 状态（不读 entry，只看 submission 自己记的）。
pub fn state_of_submission(record: &SubmissionRecord) -> RunState {
    fn unanswered(reason: &str) -> RunState {
        if matches!(reason, "aborted" | "reset") {
            RunState::Cancelled
        } else {
            RunState::Failed
        }
    }
    match &record.kind {
        SubmissionKind::Input { state } => match state {
            InputState::Queued => RunState::Queued,
            InputState::Placed { .. } => RunState::Running,
            InputState::Done { .. } => RunState::Completed,
            InputState::Unanswered { reason, .. } => unanswered(reason),
        },
        SubmissionKind::Write { state } => match state {
            WriteState::Queued => RunState::Queued,
            WriteState::Done { .. } => RunState::Completed,
            WriteState::Unanswered { reason, .. } => unanswered(reason),
        },
    }
}

/// 一个 conversation 自己的 entry 与全部 submission。
struct Log {
    entries: Vec<EntryRecord>,
    submissions: Vec<SubmissionRecord>,
}

async fn load_log(
    storage: &dyn Storage,
    conversation: ConversationRecord,
) -> Result<Log, SessionError> {
    let mut query = EntryQuery::new(conversation.id);
    query.order = Some(ScanOrder::Ascending);
    // fork 子 conversation 的可见历史带着祖先那一段；run 都在自己名下的 entry 里。
    query.min = conversation
        .parent
        .map(|parent| EntryId::new(parent.at.get() + 1));
    let mut entries = Vec::new();
    let mut cursor: Option<Cursor> = None;
    loop {
        let page = storage
            .scan_entries(&query, SCAN_PAGE, cursor.as_ref())
            .await?;
        entries.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    let query = SubmissionQuery {
        conversation: Some(conversation.id),
        status: None,
        order: Some(ScanOrder::Ascending),
    };
    let mut submissions = Vec::new();
    let mut cursor: Option<Cursor> = None;
    loop {
        let page = storage
            .scan_submissions(&query, SCAN_PAGE, cursor.as_ref())
            .await?;
        submissions.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(Log {
        entries,
        submissions,
    })
}

/// 一个 Session 的全部 conversation（按 ID 升序）。
pub(super) async fn session_conversations(
    storage: &dyn Storage,
    session: &str,
) -> Result<Vec<ConversationRecord>, SessionError> {
    let query = ConversationQuery {
        session: Some(session.to_owned()),
        order: Some(ScanOrder::Ascending),
        ..ConversationQuery::default()
    };
    let mut all = Vec::new();
    let mut cursor: Option<Cursor> = None;
    loop {
        let page = storage
            .scan_conversations(&query, SCAN_PAGE, cursor.as_ref())
            .await?;
        all.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(all),
        }
    }
}

/// 一个 conversation 投影、折叠之后的样子。
struct Folded {
    events: Vec<Event>,
    surface: Surface,
}

fn fold_log(session: &str, log: &Log) -> Folded {
    let events = legacy_events(session, &log.entries, &log.submissions);
    let surface = fold(&events);
    Folded { events, surface }
}

fn of_run<'a>(events: &'a [Event], run: &'a RunId) -> impl Iterator<Item = &'a Event> {
    events
        .iter()
        .filter(move |event| event.run.as_ref() == Some(run))
}

fn is_terminal_event(event: &Event) -> bool {
    matches!(
        event.payload,
        EventPayload::RunCompleted(_)
            | EventPayload::RunFailed(_)
            | EventPayload::RunCancelled(_)
            | EventPayload::RunAbandoned(_)
            | EventPayload::RunAbsorbed(_)
    )
}

/// 这条 run 的来源：输入 entry 是 `cron.input` 就是 cron，其余是交互。
fn source_of(log: &Log, session: &str, submission: &SubmissionRecord) -> PlanSource {
    let entry = match &submission.kind {
        SubmissionKind::Input {
            state:
                InputState::Placed { entry }
                | InputState::Done { entry, .. }
                | InputState::Unanswered {
                    entry: Some(entry), ..
                },
        }
        // 命令型 cron（与 `!`）是一条 `shell.command` write，不是输入。
        | SubmissionKind::Write {
            state: WriteState::Done { entry },
        } => log.entries.iter().find(|candidate| candidate.id == *entry),
        _ => None,
    };
    if let Some(entry) = entry
        && matches!(entry.kind.as_str(), "cron.input" | "shell.command")
        && let Some(job) = entry
            .data
            .as_ref()
            .and_then(|data| data.get("job"))
            .and_then(Value::as_str)
    {
        return PlanSource::Cron {
            job: CronJobId::from_raw(job),
            job_version: 0,
        };
    }
    PlanSource::Interactive {
        session: SessionId::from_raw(session),
    }
}

/// 一条 run 的概览。`live` 是它所在 conversation 的 `komo.live`（等重试时用）。
fn summarize(
    session: &str,
    run: &RunId,
    view: &RunView,
    folded: &Folded,
    log: &Log,
    live: &komo_runtime::durable::harness::LiveState,
) -> Option<RunSummary> {
    let submission_id = legacy_ids::submission_of(run)?;
    let submission = log
        .submissions
        .iter()
        .find(|candidate| candidate.id == submission_id)?;
    let mut mine = of_run(&folded.events, run);
    let first = mine.next();
    let created_at = of_run(&folded.events, run)
        .find(|event| matches!(event.payload, EventPayload::RunStarted(_)))
        .or(first)
        .map_or(OffsetDateTime::UNIX_EPOCH, |event| event.ts);
    let ended_at = of_run(&folded.events, run)
        .find(|event| is_terminal_event(event))
        .map(|event| event.ts.max(created_at));
    let (mut state, mut wait) = (view.status, view.wait.clone());
    if state == RunState::Running
        && let Some(control) = &live.run
        && control.inputs.first() == Some(&submission_id)
        && let Some(retry) = live.generation.as_ref().and_then(retry_wait)
    {
        state = RunState::Waiting;
        wait = Some(retry);
    }
    Some(RunSummary {
        run: run.clone(),
        session: SessionId::from_raw(session),
        state,
        wait: (state == RunState::Waiting).then_some(wait).flatten(),
        source: source_of(log, session, submission),
        rounds: view.rounds,
        created_at,
        ended_at,
    })
}

// ------------------------------------------------------------ 详情的各格（取自旧 `runs.rs`）

/// 各轮用量求和。某一项从没人报过就保持 `None`（未知不是零，§8.5）。
fn token_totals(events: &[Event], run: &RunId) -> Option<TokenTotals> {
    fn add(total: &mut Option<u64>, value: Option<u64>) {
        if let Some(value) = value {
            *total = Some(total.unwrap_or(0).saturating_add(value));
        }
    }
    let mut totals = TokenTotals::default();
    for event in of_run(events, run) {
        if let EventPayload::MessageAssistant(body) = &event.payload {
            add(&mut totals.input, body.input_tokens);
            add(&mut totals.output, body.output_tokens);
            add(&mut totals.cache_read, body.cache_read_tokens);
            add(&mut totals.cache_write, body.cache_write_tokens);
        }
    }
    (totals != TokenTotals::default()).then_some(totals)
}

/// 这条 run 期间发生的压缩 / 衰减。投影给 `context.compacted` 的 `run` 是空的（压缩不属于哪一条
/// run），所以按 seq 区间认：从这条 run 的第一帧到它的终态帧（还没结束就到最后）。
fn compactions_of(events: &[Event], run: &RunId) -> Vec<CompactionSummary> {
    fn reason(decision: &komo_kernel::compaction::CompactionDecision) -> Option<String> {
        serde_json::to_value(decision.reason)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
    }
    let Some(from) = of_run(events, run).map(|event| event.seq).min() else {
        return Vec::new();
    };
    let to = of_run(events, run)
        .find(|event| is_terminal_event(event))
        .map_or(komo_kernel::types::ids::Seq(u64::MAX), |event| event.seq);
    events
        .iter()
        .filter(|event| event.seq >= from && event.seq <= to)
        .filter_map(|event| match &event.payload {
            EventPayload::ContextCompacted(ContextCompacted::Compacted { decision, .. }) => {
                Some(CompactionSummary {
                    outcome: "compacted".into(),
                    archive_tokens: Some(decision.archive_tokens),
                    post_compaction_tokens: Some(decision.post_compaction_tokens),
                    decayed_calls: None,
                    reason: reason(decision),
                })
            }
            EventPayload::ContextCompacted(ContextCompacted::Decayed {
                calls, decision, ..
            }) => Some(CompactionSummary {
                outcome: "decayed".into(),
                archive_tokens: Some(decision.archive_tokens),
                post_compaction_tokens: Some(decision.post_compaction_tokens),
                decayed_calls: Some(calls.len() as u32),
                reason: reason(decision),
            }),
            EventPayload::ContextCompacted(ContextCompacted::Skipped {
                reason: why,
                decision,
            }) => Some(CompactionSummary {
                outcome: "skipped".into(),
                archive_tokens: decision.as_ref().map(|d| d.archive_tokens),
                post_compaction_tokens: decision.as_ref().map(|d| d.post_compaction_tokens),
                decayed_calls: None,
                reason: Some(why.clone()),
            }),
            _ => None,
        })
        .collect()
}

fn tool_name(events: &[Event], call: &komo_kernel::types::ids::ToolCallId) -> Option<String> {
    events.iter().rev().find_map(|event| match &event.payload {
        EventPayload::MessageAssistant(body) => body
            .tool_calls
            .iter()
            .find(|candidate| &candidate.call_id == call)
            .map(|candidate| candidate.name.clone()),
        _ => None,
    })
}

fn preview(surface: &Surface, call: &komo_kernel::types::ids::ToolCallId) -> Option<String> {
    surface.messages.iter().rev().find_map(|message| {
        message
            .tool_results
            .iter()
            .find(|result| &result.call == call)
            .and_then(|result| result.preview.clone())
    })
}

/// 这条 run 的模型：第一条 assistant 消息自己带的（`extra.model`，provider 回的）。受理时没有冻结
/// 快照（模型每轮按当前配置解析），所以说的是"实际答了的那个"。
fn model_of(events: &[Event], run: &RunId, log: &Log) -> Option<String> {
    of_run(events, run).find_map(|event| {
        if !matches!(event.payload, EventPayload::MessageAssistant(_)) {
            return None;
        }
        let entry = EntryId::new(legacy_ids::record_of(event.seq));
        let record = log.entries.iter().find(|candidate| candidate.id == entry)?;
        record
            .model
            .iter()
            .flatten()
            .find_map(|message| match message {
                Message::Assistant(assistant) => assistant
                    .extra
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                _ => None,
            })
    })
}

impl DurableCore {
    async fn conversation_record(
        &self,
        conversation: ConversationId,
    ) -> Result<ConversationRecord, SessionError> {
        self.storage
            .conversation(conversation)
            .await?
            .ok_or_else(|| SessionError::NotFound(format!("conversation {conversation}")))
    }

    async fn submission_of_run(&self, run: &RunId) -> Result<SubmissionRecord, SessionError> {
        let not_found = || SessionError::NotFound(format!("run {run}"));
        let id = legacy_ids::submission_of(run).ok_or_else(not_found)?;
        self.storage.submission(id).await?.ok_or_else(not_found)
    }

    /// 一个 submission 此刻的 run 状态（提交回执用）。
    pub async fn submission_state(&self, id: SubmissionId) -> Result<RunState, SessionError> {
        Ok(self
            .storage
            .submission(id)
            .await?
            .map_or(RunState::Queued, |record| state_of_submission(&record)))
    }

    /// 一个会话里没进终态的 run（会话详情的 `unfinished`、`resume` 的返回），按 `RunId` 升序。
    /// 只投影"忙"的 conversation（有 run 在跑或收件箱里排着输入）：空闲的不可能有没结束的。
    pub async fn unfinished_runs(&self, session: &str) -> Result<Vec<RunSummary>, SessionError> {
        self.session(session).await?;
        let mut out: Vec<RunSummary> = Vec::new();
        for conversation in session_conversations(&*self.storage, session).await? {
            let live = Harness::live(&*self.storage, conversation.id).await?;
            let inbox = Harness::inbox(&*self.storage, conversation.id).await?;
            let queued = inbox.items.iter().any(|item| {
                !matches!(
                    item,
                    komo_runtime::durable::harness::InboxItem::Write { .. }
                )
            });
            if live.run.is_none() && !queued {
                continue;
            }
            let log = load_log(&*self.storage, conversation).await?;
            let folded = fold_log(session, &log);
            for (run, view) in &folded.surface.runs {
                if let Some(summary) = summarize(session, run, view, &folded, &log, &live)
                    && !summary.state.is_terminal()
                {
                    out.push(summary);
                }
            }
        }
        out.sort_by_key(|summary| legacy_ids::submission_of(&summary.run).map_or(0, |id| id.get()));
        Ok(out)
    }

    /// `GET /v1/runs/{id}` 的内容。找不到 submission（含旧 UUID 形状的 RunId）是 `NotFound`。
    pub async fn run_detail(&self, run: &RunId) -> Result<RunDetail, SessionError> {
        let submission = self.submission_of_run(run).await?;
        let conversation = self.conversation_record(submission.conversation).await?;
        let session = conversation.session.clone().unwrap_or_default();
        if let SubmissionKind::Write { state } = &submission.kind {
            return self
                .write_run_detail(run, &session, &submission, state)
                .await;
        }
        let live = Harness::live(&*self.storage, conversation.id).await?;
        let log = load_log(&*self.storage, conversation).await?;
        let folded = fold_log(&session, &log);
        let view = folded
            .surface
            .runs
            .get(run)
            .ok_or_else(|| SessionError::NotFound(format!("run {run}")))?;
        let summary = summarize(&session, run, view, &folded, &log, &live)
            .ok_or_else(|| SessionError::NotFound(format!("run {run}")))?;

        let mut calls = Vec::new();
        for call_id in &view.calls {
            let Some(call) = folded.surface.calls.get(call_id) else {
                continue;
            };
            calls.push(ToolCallSummary {
                call: call_id.clone(),
                tool: tool_name(&folded.events, call_id).unwrap_or_else(|| "（未知工具）".into()),
                state: call.state,
                attempts: call.attempts,
                plan_hash: call.plan_hash.clone(),
                output: call.output.clone(),
                preview: preview(&folded.surface, call_id),
            });
        }
        let duration_ms = summary.ended_at.map(|ended| {
            (ended - summary.created_at)
                .whole_milliseconds()
                .max(0)
                .try_into()
                .unwrap_or(u64::MAX)
        });
        // 注入的记忆记在 conversation 级 `komo.memory` document 里：合成一条 `MemoryInjected`
        // 挂到触发召回的 run 上，之后的 run 沿用并标出来（同旧 `memory.injected` 的口径）。
        let (memories, recall) = match komo_runtime::memory::durable::read_memory_doc(
            &*self.storage,
            submission.conversation,
        )
        .await
        {
            Some(doc) => {
                let mut events = folded.events.clone();
                komo_runtime::memory::durable::inject_memory_events(&mut events, &doc);
                crate::http::runs::injection_of(&events, run)
            }
            None => (Vec::new(), None),
        };
        Ok(RunDetail {
            calls,
            final_message: view.final_message.clone(),
            memories,
            recall,
            model: model_of(&folded.events, run, &log),
            duration_ms,
            failure: view.reason.clone(),
            tokens: token_totals(&folded.events, run),
            compactions: compactions_of(&folded.events, run),
            summary,
        })
    }

    /// write submission 的详情：只有 `shell.command`（`!` 命令）算 run。
    async fn write_run_detail(
        &self,
        run: &RunId,
        session: &str,
        submission: &SubmissionRecord,
        state: &WriteState,
    ) -> Result<RunDetail, SessionError> {
        let entry = match state {
            WriteState::Done { entry } => {
                self.storage.entry(*entry).await?.map(|stored| stored.entry)
            }
            _ => None,
        };
        // 排着队的 write 看不到 entry：从收件箱里认它是不是 `!` 命令。
        let queued_is_shell = match state {
            WriteState::Queued => Harness::inbox(&*self.storage, submission.conversation)
                .await?
                .items
                .iter()
                .any(|item| {
                    matches!(item, komo_runtime::durable::harness::InboxItem::Write { id, entry }
                        if *id == submission.id && entry.kind == SHELL_ENTRY)
                }),
            _ => false,
        };
        let is_shell = entry
            .as_ref()
            .is_some_and(|entry| entry.kind == SHELL_ENTRY)
            || queued_is_shell;
        if !is_shell {
            return Err(SessionError::NotFound(format!("run {run}")));
        }
        let run_state = state_of_submission(submission);
        let ts = entry
            .as_ref()
            .and_then(|entry| entry.model.as_ref())
            .and_then(|model| model.first())
            .and_then(|message| match message {
                Message::User(user) => Some(time_of(user.timestamp)),
                _ => None,
            })
            .unwrap_or_else(|| self.clock.now());
        let failed = entry
            .as_ref()
            .and_then(|entry| entry.data.as_ref())
            .and_then(|data| data.get("status"))
            .and_then(Value::as_str)
            .is_some_and(|status| status != "completed");
        let view = entry.as_ref().and_then(Self::shell_view);
        let (state_out, failure) = match (run_state, failed) {
            (RunState::Completed, true) => (RunState::Failed, view.clone()),
            (other, _) => (other, None),
        };
        let ended = state_out.is_terminal().then_some(ts);
        Ok(RunDetail {
            summary: RunSummary {
                run: run.clone(),
                session: SessionId::from_raw(session),
                state: state_out,
                wait: None,
                source: PlanSource::Interactive {
                    session: SessionId::from_raw(session),
                },
                rounds: 0,
                created_at: ts,
                ended_at: ended,
            },
            calls: Vec::new(),
            final_message: view,
            memories: Vec::new(),
            recall: None,
            model: None,
            duration_ms: entry
                .as_ref()
                .and_then(|entry| entry.data.as_ref())
                .and_then(|data| data.get("elapsedMs"))
                .and_then(Value::as_u64),
            failure,
            tokens: None,
            compactions: Vec::new(),
        })
    }

    /// `POST /v1/runs/{id}/cancel`：排队中的单条 = `Harness::withdraw`；已经在跑的 = `abort_run`
    /// （决定 8：只停当前一轮，不撤回收件箱）。已终态的原样返回（重复取消是幂等的）。
    pub async fn cancel_run_id(&self, run: &RunId) -> Result<RunState, SessionError> {
        let submission = self.submission_of_run(run).await?;
        let conversation = submission.conversation;
        let mut abort = matches!(
            &submission.kind,
            SubmissionKind::Input {
                state: InputState::Placed { .. }
            }
        );
        if state_of_submission(&submission) == RunState::Queued {
            match self
                .harness
                .withdraw(&self.engine, submission.id, Some(conversation))
                .await?
            {
                WithdrawResult::AlreadyPlaced => abort = true,
                WithdrawResult::Aborted | WithdrawResult::Settled | WithdrawResult::NotFound => {}
            }
        }
        if abort {
            self.harness.abort_run(&self.engine, conversation).await?;
        }
        self.submission_state(submission.id).await
    }
}
