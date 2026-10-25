//! 从 entry 与 submission 合成旧事件词汇（D5 W1a，`docs/storage-durable-d5.md` §1 / §5）：纯函数。
//!
//! 协议在 D5 冻结，客户端读的还是 [`Event`]。gateway 把提交订阅里的记录交给这里，得到旧事件；
//! SSE 补读用同一个函数从空状态把整个 Session 的记录喂一遍，所以**补读与直播的帧逐个相同**
//! （`legacy_events_backfill_equals_live_projection`）。
//!
//! # 接口
//!
//! [`Projection`] 是投影状态；[`Projection::project`] 吃"一批新记录"（[`Record`]）吐"这批产出的帧"，
//! 状态原地推进。批内按记录 ID 排序（entry 与 submission 共用一个 ID 命名空间，不会撞），批与批之间
//! 按提交先后喂。[`legacy_events`] 是补读：整个 Session 的 entry 与 submission（当前状态）一次喂完。
//!
//! 记录的约定：
//!
//! - **submission** 每次有写（新建、放置、结算）都可以喂，状态只用来记"输入 entry ↔ submission"，
//!   `RunAccepted` / `RunQueued` 只在第一次见到这个 submission 时出——所以直播（新建时喂一次，之后
//!   每次改写再喂）与补读（只喂最终状态）出同样的帧。write submission 不出帧。
//! - **entry** 只喂一次；ID 不大于已处理的最大 entry ID 的被当作重放丢掉（幂等）。
//! - 补读要**从头**喂：run 的归属靠"输入放置之后到结算之前"的顺序折出来。只喂游标之后的记录会让
//!   游标时仍开着的 run 的 entry 丢失归属（`run` 为 `None`）；要按游标截的，调用方先投影全部再丢
//!   `seq <= cursor` 的帧。
//!
//! # 映射
//!
//! | 记录 | 帧（`seq(记录 ID, k)`） |
//! |---|---|
//! | 输入 submission S 第一次出现 | `RunAccepted`（S,0）、`RunQueued`（S,1） |
//! | 输入 entry E，当时没有"还开着"的 run | `RunStarted`（E,0）、`MessageUser`（E,1），RunId = 这个 submission |
//! | 输入 entry E，当前 run 还开着 | `RunAbsorbed{into}`（E,0，`run` 是被并的那条）、`MessageUser`（E,1，`run` 是宿主） |
//! | assistant entry A | `MessageAssistant`（A,0）；第 i 个调用 `ToolPlanned`（A,1+2i）、`ToolStarted`（A,2+2i） |
//! | 工具结果 entry T | `ToolResult`（T,0）…… 每条结果消息一帧 |
//! | `komo.settled` X（run 的首个输入） | `RunCompleted` / `RunCancelled` / `RunFailed`（X,0） |
//! | `komo.settled` X（run 里并进来的别的输入） | 无 |
//! | `komo.settled` X（从没放置过的排队输入，已撤回） | `RunCancelled`（`aborted` / `reset`）或 `RunFailed`（X,0），`run` 是它自己 |
//! | `head: self` 的 entry（`komo.reset`） | `ConversationBoundary`（R,0） |
//! | `komo.compaction` | `ContextCompacted::Compacted`（C,0） |
//! | `komo.decay` | `ContextCompacted::Decayed`（D,0） |
//! | `model.change` | `ConfigChanged`（M,0） |
//! | 别的 entry：有 `model` 里的 user 消息、kind 不是 `komo.*`（`komo.user` 除外）与 `branch.summary` | `MessageUser`（E,0），`run` 为空——`!` 命令这类"不开启 run 的用户消息" |
//! | 其余（`komo.system`、`label`、`session.info`、`branch.summary`、`context.edit`、不认识的 kind） | **不出帧**。不认识的 kind 在 pi 树里是 `Custom`（见 [`pi_tree`](super::pi_tree)），事件流里没有对应词汇，宁可不出也不出一个误导的帧 |
//!
//! 与 §5 表的两处差别：
//!
//! 1. `RunQueued` 每个输入 submission 都出（旧事件的顺序一直是 accepted → queued → started），不只"排队时"。
//!    直播第一次见到 submission 时它是 `queued` 还是已 `placed` 取决于忙闲，补读见到的却是最终状态，
//!    按状态决定出不出就会让两边的帧不一样；
//! 2. `RunAccepted` 没有输入正文、`peer`、`model`：submission 记录里没有，正文在放置它的 entry 上
//!    （`MessageUser`），来源对端在 entry 的 `data.peer`（`watch` 直接读 entry）。
//!
//! # run 的归属
//!
//! RunId = run **首个输入**的 submission ID。一个 conversation 里"当前 run"从首个输入 entry 放置起，
//! 到它的 `komo.settled` 为止；其间放置的别的输入并进它。有两种提前"合上"当前 run 的信号，
//! 此后放置的输入开新 run（旧 run 的 `komo.settled` 之后到，按 submission 找回它）：
//!
//! - 它刚追加了没有工具调用的 assistant entry，或停止原因是 `error` / `aborted`：这是回答或失败，
//!   run 到此为止。`generation::answer` 先放置收件箱里的后继输入、后追加 `komo.settled`，所以后继的
//!   输入 entry 的 ID 比旧 run 的结算还小；
//! - 它之后追加了 `head: self` 的 entry（排队的 reset 把 `postTools` 变成 `final`，当前 run 以
//!   `reset` 收场，后继输入也排在结算之前）。
//!
//! 一条 run 吸收了 steer，有几个输入就有几条 `komo.settled`（同一个 `answer`）：只在**首个输入**的那条
//! 出终态帧，别的不出，所以一条 run 恰好一个终态事件。
//!
//! # 其余
//!
//! - `Seq` 在一个 Session 内严格递增：记录按 ID 序，同一记录的帧 `k` 递增（见 [`legacy_ids`]）。
//! - `event_id` = `evt-{seq}`，`ts` 取记录里第一条消息的毫秒时间戳，没有的沿用前一条记录的
//!   （第一条之前是 0）。
//! - 工具调用的 `call_id` 与 `provider_call_id` 都是消息里的 `toolCallId`；`ToolResult` 的输出引用取
//!   工具结果 entry 的 `data.output`，没有就合成一个占位路径。
//! - `AssistantDelta`、`RunStatus` 不在这里：前者来自 `komo.live` 文档，后者由 hub 照旧派生。

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use time::OffsetDateTime;

use super::compaction::{COMPACTION_ENTRY_KIND, DECAY_ENTRY_KIND, SUMMARY_PREFIX, SUMMARY_SUFFIX};
use super::ids::{ConversationId, EntityId, EntryId, SubmissionId};
use super::legacy_ids::{run_id, seq};
use super::message::{AssistantMessage, ContentBlock, Message, StopReason};
use super::pi_tree::{BRANCH_SUMMARY_KIND, MODEL_CHANGE_KIND, entry_millis, user_text};
use super::records::{EditAction, EntryRecord, InputState, SubmissionKind, SubmissionRecord};
use crate::compaction::{CompactionDebt, CompactionDecision, CompactionReason};
use crate::events::{
    ConfigChanged, ContextCompacted, ConversationBoundary, EVENT_FORMAT_VERSION, Event,
    EventPayload, MessageAssistant, MessageUser, RunAbsorbed, RunAccepted, RunCancelled,
    RunCompleted, RunFailed, RunQueued, RunStarted, ToolPlanned, ToolResult, ToolStarted,
};
use crate::types::{
    AttemptId, ContentHash, EventId, ExecutorId, OutputRef, PlanHash, PlanSource, RequestKey,
    RunId, SessionId, TokenUsage, ToolCallId, ToolResultBody, ToolResultStatus,
};
use crate::types::{ContentRef, PREVIEW_LIMIT_BYTES, ToolCallRequest};

/// `komo.settled` 的 kind（与 runtime `harness::entries::SETTLED` 一致，kernel 不依赖 runtime）。
pub const SETTLED_ENTRY_KIND: &str = "komo.settled";
const USER_ENTRY_KIND: &str = "komo.user";
const ASSISTANT_ENTRY_KIND: &str = "komo.assistant";
const TOOL_RESULT_ENTRY_KIND: &str = "komo.tool-result";

/// 喂给投影的一条记录。
#[derive(Debug, Clone, Copy)]
pub enum Record<'a> {
    Entry(&'a EntryRecord),
    Submission(&'a SubmissionRecord),
}

impl Record<'_> {
    /// 记录 ID（entry 与 submission 同一个命名空间）。
    pub fn id(&self) -> u64 {
        match self {
            Record::Entry(entry) => entry.id.raw(),
            Record::Submission(submission) => submission.id.raw(),
        }
    }
}

/// 补读：整个 Session 的 entry（任意次序）与 submission（当前状态）从空状态投影一遍。
pub fn legacy_events(
    session: &str,
    entries: &[EntryRecord],
    submissions: &[SubmissionRecord],
) -> Vec<Event> {
    let records: Vec<Record<'_>> = entries
        .iter()
        .map(Record::Entry)
        .chain(submissions.iter().map(Record::Submission))
        .collect();
    Projection::new(session).project(&records)
}

/// 一条还没结算的 run。
#[derive(Debug, Clone)]
struct RunState {
    rounds: u32,
    /// 回答 / 失败 / reset 已经出现：之后放置的输入不再并进它。
    closing: bool,
    /// 最近一条 assistant entry 与它的文字，`done` 的 `answer` 指向它。
    last_answer: Option<(EntryId, String)>,
}

/// 投影状态：一个 Session 的所有 conversation 共用。
#[derive(Debug, Clone)]
pub struct Projection {
    session: SessionId,
    last_entry: u64,
    last_ms: i64,
    seen_submissions: HashSet<SubmissionId>,
    /// 输入 entry → 放置它的 submission。
    placed: HashMap<EntryId, SubmissionId>,
    /// 输入 submission → 它所在 run 的首个输入。
    member: HashMap<SubmissionId, SubmissionId>,
    /// 还没出终态的 run，按首个输入索引。
    runs: HashMap<SubmissionId, RunState>,
    /// 每个 conversation 里最近开的那条 run。
    current: HashMap<ConversationId, SubmissionId>,
}

impl Projection {
    pub fn new(session: &str) -> Self {
        Self {
            session: SessionId::from_raw(session),
            last_entry: 0,
            last_ms: 0,
            seen_submissions: HashSet::new(),
            placed: HashMap::new(),
            member: HashMap::new(),
            runs: HashMap::new(),
            current: HashMap::new(),
        }
    }

    /// 投影一批新记录，返回这批产出的帧（`seq` 严格递增），状态推进。
    pub fn project(&mut self, records: &[Record<'_>]) -> Vec<Event> {
        let mut sorted: Vec<&Record<'_>> = records.iter().collect();
        sorted.sort_by_key(|record| record.id());
        let mut out = Vec::new();
        for record in sorted {
            match record {
                Record::Submission(submission) => self.submission(submission, &mut out),
                Record::Entry(entry) => {
                    // 重放：entry ID 全局单调，不大于已处理的就是见过的。
                    if entry.id.get() > self.last_entry {
                        self.last_entry = entry.id.get();
                        self.entry(entry, &mut out);
                    }
                }
            }
        }
        out
    }

    // ------------------------------------------------------------ submission

    fn submission(&mut self, record: &SubmissionRecord, out: &mut Vec<Event>) {
        let SubmissionKind::Input { state } = &record.kind else {
            return;
        };
        let placed = match state {
            InputState::Placed { entry } | InputState::Done { entry, .. } => Some(*entry),
            InputState::Unanswered { entry, .. } => *entry,
            InputState::Queued => None,
        };
        if let Some(entry) = placed {
            self.placed.insert(entry, record.id);
        }
        if !self.seen_submissions.insert(record.id) {
            return;
        }
        let run = run_id(record.id);
        let key = record
            .request_id
            .clone()
            .unwrap_or_else(|| format!("submission-{}", record.id));
        let accepted = Frame::at(self, record.id.raw(), self.last_ms);
        let first = accepted.emit(
            out,
            0,
            Some(run.clone()),
            EventPayload::RunAccepted(RunAccepted {
                input_hash: ContentHash::of_str(&key),
                request_key: RequestKey::new(key),
                text: None,
                text_ref: None,
                source: PlanSource::Interactive {
                    session: self.session.clone(),
                },
                peer: None,
                model: None,
                effort: None,
                delegate: None,
                snapshot: None,
                interject: false,
            }),
        );
        accepted.emit(
            out,
            1,
            Some(run),
            EventPayload::RunQueued(RunQueued { input_ref: first }),
        );
    }

    // ------------------------------------------------------------------ entry

    fn entry(&mut self, entry: &EntryRecord, out: &mut Vec<Event>) {
        self.last_ms = entry_millis(entry).unwrap_or(self.last_ms);
        let frame = Frame::at(self, entry.id.raw(), self.last_ms);
        if entry.kind == SETTLED_ENTRY_KIND {
            return self.settled(entry, &frame, out);
        }
        if let Some(&submission) = self.placed.get(&entry.id) {
            return self.input(entry, submission, &frame, out);
        }
        if entry.kind == COMPACTION_ENTRY_KIND {
            return self.compaction(entry, &frame, out);
        }
        if entry.head == Some(entry.id) {
            if let Some(run) = self.open_run(entry.conversation) {
                run.closing = true;
            }
            frame.emit(
                out,
                0,
                None,
                EventPayload::ConversationBoundary(ConversationBoundary { by: None }),
            );
            return;
        }
        match entry.kind.as_str() {
            DECAY_ENTRY_KIND => self.decay(entry, &frame, out),
            MODEL_CHANGE_KIND => {
                frame.emit(
                    out,
                    0,
                    None,
                    EventPayload::ConfigChanged(ConfigChanged {
                        keys: vec!["model".to_owned()],
                        model: data_str(entry, "modelId")
                            .or_else(|| data_str(entry, "model"))
                            .map(str::to_owned),
                        effort: None,
                        reason: MODEL_CHANGE_KIND.to_owned(),
                    }),
                );
            }
            ASSISTANT_ENTRY_KIND => self.assistant(entry, &frame, out),
            TOOL_RESULT_ENTRY_KIND => self.tool_result(entry, &frame, out),
            kind => {
                let plain = kind == USER_ENTRY_KIND
                    || !(kind.starts_with("komo.") || kind == BRANCH_SUMMARY_KIND);
                if plain
                    && entry.head.is_none()
                    && let Some(text) = entry.model.iter().flatten().find_map(user_text)
                {
                    frame.emit(
                        out,
                        0,
                        None,
                        EventPayload::MessageUser(MessageUser {
                            text: Some(text),
                            text_ref: None,
                        }),
                    );
                }
            }
        }
    }

    /// conversation 里最近开的、还没结算的 run。
    fn open_run(&mut self, conversation: ConversationId) -> Option<&mut RunState> {
        let first = *self.current.get(&conversation)?;
        self.runs.get_mut(&first)
    }

    fn input(
        &mut self,
        entry: &EntryRecord,
        submission: SubmissionId,
        frame: &Frame,
        out: &mut Vec<Event>,
    ) {
        let text = entry.model.iter().flatten().find_map(user_text);
        let own = run_id(submission);
        let host = self
            .current
            .get(&entry.conversation)
            .copied()
            .filter(|first| self.runs.get(first).is_some_and(|run| !run.closing));
        let message = |text: Option<String>| {
            EventPayload::MessageUser(MessageUser {
                text,
                text_ref: None,
            })
        };
        match host {
            Some(first) => {
                self.member.insert(submission, first);
                let host = run_id(first);
                frame.emit(
                    out,
                    0,
                    Some(own),
                    EventPayload::RunAbsorbed(RunAbsorbed { into: host.clone() }),
                );
                frame.emit(out, 1, Some(host), message(text));
            }
            None => {
                self.member.insert(submission, submission);
                self.runs.insert(
                    submission,
                    RunState {
                        rounds: 0,
                        closing: false,
                        last_answer: None,
                    },
                );
                self.current.insert(entry.conversation, submission);
                frame.emit(
                    out,
                    0,
                    Some(own.clone()),
                    EventPayload::RunStarted(RunStarted {
                        executor: ExecutorId::from_raw("komo"),
                        generation: 1,
                    }),
                );
                frame.emit(out, 1, Some(own), message(text));
            }
        }
    }

    fn settled(&mut self, entry: &EntryRecord, frame: &Frame, out: &mut Vec<Event>) {
        let Some(data) = entry.data.as_ref() else {
            return;
        };
        let Some(submission) = data.get("submission").and_then(Value::as_u64) else {
            return;
        };
        let submission = SubmissionId::new(submission);
        let (run, rounds, answer_text) = match self.member.get(&submission).copied() {
            // 并进别人 run 的输入：终态由首个输入那条出。
            Some(first) if first != submission => return,
            Some(first) => {
                if self.current.get(&entry.conversation) == Some(&first) {
                    self.current.remove(&entry.conversation);
                }
                let state = self.runs.remove(&first);
                let answer = data.get("answer").and_then(Value::as_u64).map(EntryId::new);
                let text = state.as_ref().and_then(|state| {
                    state
                        .last_answer
                        .as_ref()
                        .filter(|(id, _)| Some(*id) == answer)
                        .map(|(_, text)| text.clone())
                });
                (first, state.map_or(0, |state| state.rounds), text)
            }
            // 从没放置过（排队中被撤回）：它自己就是一条 run。
            None => (submission, 0, None),
        };
        let status = data.get("status").and_then(Value::as_str).unwrap_or("");
        let reason = data.get("reason").and_then(Value::as_str).unwrap_or("");
        let payload = if status == "done" {
            EventPayload::RunCompleted(RunCompleted {
                final_message: answer_text,
                final_message_ref: None,
                rounds,
            })
        } else if matches!(reason, "aborted" | "reset") {
            EventPayload::RunCancelled(RunCancelled { by: None })
        } else {
            let mut text = if reason.is_empty() {
                "unanswered".to_owned()
            } else {
                reason.to_owned()
            };
            match data.get("detail") {
                Some(Value::String(detail)) if !detail.is_empty() => {
                    text = format!("{text}: {detail}");
                }
                Some(Value::Null) | None => {}
                Some(detail) => text = format!("{text}: {detail}"),
            }
            EventPayload::RunFailed(RunFailed { reason: text })
        };
        frame.emit(out, 0, Some(run_id(run)), payload);
    }

    fn assistant(&mut self, entry: &EntryRecord, frame: &Frame, out: &mut Vec<Event>) {
        for message in entry.model.iter().flatten() {
            let Message::Assistant(assistant) = message else {
                continue;
            };
            let text = assistant_text(assistant);
            let calls: Vec<ToolCallRequest> = assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall {
                        id,
                        name,
                        arguments,
                        ..
                    } => Some(ToolCallRequest {
                        call_id: ToolCallId::from_raw(id),
                        provider_call_id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                        arguments_ref: None,
                    }),
                    _ => None,
                })
                .collect();
            let ends_run = calls.is_empty()
                || matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                );
            let conversation = entry.conversation;
            let run = self.current.get(&conversation).copied();
            let run_label = run.map(run_id);
            let state = run.and_then(|first| self.runs.get_mut(&first));
            let silent = assistant.stop_reason.excluded_from_context()
                && text.is_empty()
                && calls.is_empty();
            let mut round = 1;
            if let Some(state) = state {
                state.last_answer = Some((entry.id, text.clone()));
                if ends_run {
                    state.closing = true;
                }
                if !silent {
                    state.rounds += 1;
                    round = state.rounds;
                }
            }
            if silent {
                continue;
            }
            let usage = usage_of(assistant);
            frame.emit(
                out,
                0,
                run_label.clone(),
                EventPayload::MessageAssistant(MessageAssistant {
                    round,
                    text: (!text.is_empty()).then_some(text),
                    text_ref: None,
                    tool_calls: calls.clone(),
                    provider_blocks: assistant.extra.get("providerBlocks").cloned(),
                    input_tokens: usage.input,
                    output_tokens: usage.output,
                    cache_read_tokens: usage.cache_read,
                    cache_write_tokens: usage.cache_write,
                    reasoning_tokens: usage.reasoning,
                }),
            );
            if assistant.stop_reason.excluded_from_context() {
                continue;
            }
            for (index, call) in calls.iter().enumerate() {
                let hash = ContentHash::of_str(&format!(
                    "{}\n{}\n{}",
                    call.provider_call_id, call.name, call.arguments
                ));
                let plan_hash = PlanHash::from_raw(hash.as_str());
                let planned = frame.emit(
                    out,
                    1 + 2 * index as u32,
                    run_label.clone(),
                    EventPayload::ToolPlanned(ToolPlanned {
                        call_id: call.call_id.clone(),
                        plan_hash: plan_hash.clone(),
                        plan: None,
                        plan_ref: None,
                    }),
                );
                frame.emit(
                    out,
                    2 + 2 * index as u32,
                    run_label.clone(),
                    EventPayload::ToolStarted(ToolStarted {
                        call_id: call.call_id.clone(),
                        attempt_id: AttemptId::from_raw("attempt-1"),
                        plan_ref: planned,
                        plan_hash,
                    }),
                );
            }
        }
    }

    fn tool_result(&mut self, entry: &EntryRecord, frame: &Frame, out: &mut Vec<Event>) {
        let run = self.current.get(&entry.conversation).copied().map(run_id);
        let data = entry.data.as_ref();
        let attempt = data
            .and_then(|data| data.get("attempt"))
            .and_then(Value::as_u64)
            .unwrap_or(1);
        let elapsed_ms = data
            .and_then(|data| data.get("elapsedMs"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let content_ref = |key: &str| -> Option<ContentRef> {
            serde_json::from_value(data?.get(key)?.clone()).ok()
        };
        let mut index = 0;
        for message in entry.model.iter().flatten() {
            let Message::ToolResult(result) = message else {
                continue;
            };
            let status = data
                .and_then(|data| data.get("status"))
                .and_then(|status| serde_json::from_value(status.clone()).ok())
                .unwrap_or(if result.is_error {
                    ToolResultStatus::Failed
                } else {
                    ToolResultStatus::Completed
                });
            let text = result
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let output = data
                .and_then(|data| data.get("output"))
                .and_then(|output| serde_json::from_value::<OutputRef>(output.clone()).ok())
                .unwrap_or_else(|| {
                    OutputRef(ContentRef {
                        path: format!(
                            "tool-output/{}/{}/{attempt}/output.json",
                            entry.conversation, result.tool_call_id
                        ),
                        size: 0,
                        hash: ContentHash::of_str(""),
                        pointer: None,
                    })
                });
            let body = ToolResultBody {
                status,
                result: Value::Null,
                error: (status != ToolResultStatus::Completed).then(|| text.clone()),
                exit_code: None,
                artifacts: Vec::new(),
                images: Vec::new(),
                preview: Some(text.clone()),
            };
            frame.emit(
                out,
                index,
                run.clone(),
                EventPayload::ToolResult(ToolResult {
                    body: Some(Box::new(body)),
                    call_id: ToolCallId::from_raw(&result.tool_call_id),
                    attempt_id: AttemptId::from_raw(format!("attempt-{attempt}")),
                    status,
                    output_ref: output,
                    elapsed_ms,
                    preview: (!text.is_empty()).then(|| truncate(&text, PREVIEW_LIMIT_BYTES)),
                    stdout: content_ref("stdout"),
                    stderr: content_ref("stderr"),
                    attempt_state: None,
                }),
            );
            index += 1;
        }
    }

    fn compaction(&mut self, entry: &EntryRecord, frame: &Frame, out: &mut Vec<Event>) {
        let summary = entry
            .model
            .iter()
            .flatten()
            .find_map(user_text)
            .map(|text| {
                text.strip_prefix(SUMMARY_PREFIX)
                    .and_then(|rest| rest.strip_suffix(SUMMARY_SUFFIX))
                    .map_or(text.clone(), str::to_owned)
            });
        frame.emit(
            out,
            0,
            None,
            EventPayload::ContextCompacted(ContextCompacted::Compacted {
                first_kept: seq(entry.head.unwrap_or(entry.id), 0),
                summary,
                summary_ref: None,
                decision: decision_of(entry),
                debt: debt_of(entry),
                usage: TokenUsage::default(),
            }),
        );
    }

    fn decay(&mut self, entry: &EntryRecord, frame: &Frame, out: &mut Vec<Event>) {
        let calls = entry
            .edits
            .iter()
            .filter_map(|edit| match &edit.action {
                EditAction::Replace { messages } => {
                    messages.iter().find_map(|message| match message {
                        Message::ToolResult(result) => {
                            Some(ToolCallId::from_raw(&result.tool_call_id))
                        }
                        _ => None,
                    })
                }
                EditAction::Omit => None,
            })
            .collect();
        frame.emit(
            out,
            0,
            None,
            EventPayload::ContextCompacted(ContextCompacted::Decayed {
                calls,
                decision: decision_of(entry),
                debt: debt_of(entry),
            }),
        );
    }
}

/// 一条记录产出帧的小工具：固定 session、记录 ID 与时间。
struct Frame {
    session: SessionId,
    record: u64,
    ts: OffsetDateTime,
}

impl Frame {
    fn at(projection: &Projection, record: u64, millis: i64) -> Self {
        Self {
            session: projection.session.clone(),
            record,
            ts: OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
                .unwrap_or(OffsetDateTime::UNIX_EPOCH),
        }
    }

    /// 产出第 `k` 帧，返回它的 `EventId`。
    fn emit(
        &self,
        out: &mut Vec<Event>,
        k: u32,
        run: Option<RunId>,
        payload: EventPayload,
    ) -> EventId {
        let seq = seq(EntryId::new(self.record), k);
        let event_id = EventId::from_raw(format!("evt-{}", seq.0));
        out.push(Event {
            v: EVENT_FORMAT_VERSION,
            seq,
            event_id: event_id.clone(),
            session: self.session.clone(),
            run,
            ts: self.ts,
            payload,
            entry: None,
        });
        event_id
    }
}

fn data_str<'a>(entry: &'a EntryRecord, key: &str) -> Option<&'a str> {
    entry.data.as_ref()?.get(key)?.as_str()
}

fn assistant_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn usage_of(message: &AssistantMessage) -> TokenUsage {
    let read = |key: &str| message.extra.get("usage")?.get(key)?.as_u64();
    TokenUsage {
        input: read("input"),
        output: read("output"),
        reasoning: read("reasoning"),
        cache_read: read("cacheRead"),
        cache_write: read("cacheWrite"),
    }
}

/// 压缩 / 衰减 entry 的 `data.decision`；没有就是一份全零的占位。
fn decision_of(entry: &EntryRecord) -> CompactionDecision {
    entry
        .data
        .as_ref()
        .and_then(|data| data.get("decision"))
        .and_then(|decision| serde_json::from_value(decision.clone()).ok())
        .unwrap_or(CompactionDecision {
            write_tokens: 0,
            archive_tokens: 0,
            memo_tokens: 0,
            post_compaction_tokens: 0,
            context_tokens: 0,
            horizon: None,
            breakeven_requests: None,
            combined_breakeven_requests: None,
            effective_horizon_requests: None,
            cache_write_read_ratio: None,
            incremental_cache_cost_ratio: None,
            prior_compaction_count: 0,
            requests_since_last_compaction: None,
            carried_debt_tokens: 0.0,
            cache_debt_repayment_tokens: 0,
            compact: true,
            reason: CompactionReason::Economic,
            context_overflow: false,
        })
}

fn debt_of(entry: &EntryRecord) -> CompactionDebt {
    entry
        .data
        .as_ref()
        .and_then(|data| data.get("debt"))
        .and_then(|debt| serde_json::from_value(debt.clone()).ok())
        .unwrap_or_default()
}

/// 按字符边界截到最多 `limit` 字节。
fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut cut = limit;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text[..cut].to_owned()
}

#[cfg(test)]
mod tests;
