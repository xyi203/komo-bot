//! `komo.compaction`（`docs/storage-durable.md` §4.5、§12.4；pi-durable spec §8.7，
//! `harness/compaction.ts`）：把上下文里一段旧的前缀换成摘要。
//!
//! 同一个任务，三种起法，只有归属和落点不同：
//!
//! | 谁起 | 所有者 | background | 生成等它 | 落点 |
//! |---|---|---|---|---|
//! | `Harness::compact`（手动） | conversation | 否 | 否 | write submission |
//! | 生成越过后台阈值 | conversation | 是 | 否 | write submission |
//! | 生成越过阻塞阈值，或上下文超长 | 那个生成 | 否 | 是 | 直接追加 |
//!
//! 只有阻塞的压缩在 run 等着它的时候跑；其余的从不拿 run 的控制权：conversation 在它们摘要的
//! 时候照常工作，摘要像任何被动的 write 一样在 boundary 放置（过期了——head 指到当前范围起点
//! 之前——就 `unanswered{stale}`）。
//!
//! ```json
//! { "phase": "select" }
//! { "phase": "summarize", "request": { "attempt": 1, "model": {…}, "system": "…", "instructions": "…",
//!                                      "retry": {…}, "maxTokens": 13107, "tail": 41, "firstKept": 30,
//!                                      "decision": {…CompactionDecision…} } }
//! { "phase": "retry", "until": 1761206400000, "request": {…同上…} }
//! ```
//!
//! - `select`：问钩子拿设置与摘要模型，切点（`select_cut`）；没有可收的就 `completed {}`。否则一次
//!   提交把摘要请求**钉**进 checkpoint（模型、提示、`tail`、`firstKept`），重启后重发的是同一个请求。
//! - `summarize`：从 `tail`（不可变）处重新推导出要概括的消息，序列化成 `<conversation>` 文本，
//!   发无状态的 [`ModelStream`](crate::llm::ModelStream)——**没有工具，两条消息**：系统提示与
//!   这段文本。干净的 `stop` 且有文字、没有调用 = 摘要，一次提交里放置并 `completed`；可重试的
//!   错误按有界退避进 `retry`；其余（含被截断的 `length`，摘要不完整）`failed`。
//! - 失败（`model_error` / `no_model`）的提交移除状态、记下 `komo.live.compactionFailure`；调度
//!   器写的 `faulted` / `orphaned` 也一样（`settle_scheduler_outcome`）。只有真的产出摘要才清它。
//!
//! 摘要 entry：`kind = komo.compaction`，`head` = 第一条保留的 entry，`model` = 包好的摘要 user
//! 消息，`data = { reason, debt, decision }`——`debt` 是旧实现记在 `context.compacted` 里的缓存债
//! （[`online_state`](komo_kernel::durable::compaction::online_state) 靠它折账），`decision` 的
//! `context_overflow` 标明这次是超长救回来的。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::compaction::{CompactionDecision, CompactionInput, decide};
use komo_kernel::durable::compaction::{
    COMPACTION_ENTRY_KIND, compaction_data, estimate_messages_tokens, select_cut,
    serialize_conversation, summarized_messages, wrap_summary,
};
use komo_kernel::durable::task::RunningTask;
use komo_kernel::durable::{
    AssistantMessage, Content, ContentBlock, ConversationId, EntryId, Message, StopReason,
    SystemMessage, TaskId,
};
use komo_kernel::types::model::ModelConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::COMPACTION_KIND;
use super::super::generation::GenerationDefinition;
use super::super::hooks::{HarnessDeps, HookError, RetryPolicy};
use super::super::inbox::{self, Admitted, QueueModes, SubmissionDraft, WriteDraft, WriteHead};
use super::super::live::{self, CompactionFailure, CompactionStatus, LiveState, RetryLive};
use super::super::outcome::{aborted, completed, failed, running};
use super::plan::base_input;
use super::setup::CompactionSetup;
use crate::durable::error::{DurableError, TaskError};
use crate::durable::kind::TaskKind;
use crate::durable::runtime::TaskRuntime;
use crate::durable::tx::{DraftHead, EntryDraft, Ownership, TaskOptions, Tx};
use crate::llm::{ModelRequest, RequestOptions, retry_cause};

/// 为什么压。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Reason {
    /// `Harness::compact`。
    Manual,
    /// 生成越过了阻塞或后台阈值。
    Threshold,
    /// provider 以上下文超长拒了请求。
    Overflow,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Manual => "manual",
            Reason::Threshold => "threshold",
            Reason::Overflow => "overflow",
        }
    }
}

/// 任务输入。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Input {
    pub reason: Reason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// 起它的那个决策（缓存账）；手动压缩没有，`select` 现算。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<CompactionDecision>,
}

impl Input {
    pub fn new(reason: Reason, decision: Option<CompactionDecision>) -> Self {
        Self {
            reason,
            instructions: None,
            decision,
        }
    }
}

/// 钉住的摘要请求（spec 的 `SummaryRequest`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryRequest {
    pub attempt: u32,
    pub model: ModelConfig,
    pub system: String,
    pub instructions: String,
    pub retry: RetryPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// 请求里 system / user 消息的时间戳：钉在 checkpoint 里，重发的字节才逐字节相同。
    pub created_at: i64,
    /// 选切点时上下文的最新 entry。
    pub tail: EntryId,
    /// 第一条原样保留的 entry：摘要 entry 的 `head`。
    pub first_kept: EntryId,
    pub decision: CompactionDecision,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "phase",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Checkpoint {
    Select,
    Summarize { request: SummaryRequest },
    Retry { until: i64, request: SummaryRequest },
}

fn encode(checkpoint: &Checkpoint) -> Value {
    serde_json::to_value(checkpoint).expect("Checkpoint 可序列化")
}

fn decode(task: &RunningTask) -> Result<Checkpoint, TaskError> {
    serde_json::from_value(task.checkpoint.clone())
        .map_err(|e| TaskError::new(format!("compaction checkpoint is malformed: {e}")))
}

fn input_of(task: &RunningTask) -> Result<Input, TaskError> {
    serde_json::from_value(task.input().clone())
        .map_err(|e| TaskError::new(format!("compaction input is malformed: {e}")))
}

/// 摘要请求允许的输出上限：`reserve` 的八成（pi 的 `0.8 * reserveTokens`）。
fn max_summary_tokens(setup: &CompactionSetup) -> u32 {
    (setup.settings.economics.window_reserve_tokens as f64 * 0.8) as u32
}

/// 压缩任务。克隆便宜。
#[derive(Clone)]
pub struct CompactionTask {
    deps: Arc<HarnessDeps>,
}

impl CompactionTask {
    pub fn new(deps: Arc<HarnessDeps>) -> Self {
        Self { deps }
    }

    fn modes(&self) -> QueueModes {
        let settings = self.deps.settings();
        QueueModes {
            steering: settings.steering_mode,
            follow_up: settings.follow_up_mode,
        }
    }

    // ------------------------------------------------------------------ select

    async fn select(&self, rt: &TaskRuntime, task: &RunningTask) -> Result<(), TaskError> {
        let conversation = rt.conversation();
        let input = input_of(task)?;
        let setup = match self.deps.hooks.compaction(conversation).await {
            Ok(setup) => setup,
            Err(HookError::NoModel(message)) => {
                return self.fail(rt, input.reason, "no_model", message).await;
            }
            Err(HookError::Failed(message)) => return Err(TaskError::new(message)),
        };
        let context = rt.context_snapshot(conversation).await?;
        let view = &context.view;
        let Some(cut) = select_cut(&view, setup.settings.keep_recent_tokens) else {
            return self.complete(rt, None).await;
        };
        let decision = match input.decision {
            Some(decision) => decision,
            None => {
                // 手动压缩：没有谁替它算过账，按这个切点现算一份留档。
                let online = context.online_state(None);
                let tokens = estimate_messages_tokens(&view.messages)
                    .max(online.last_context_tokens.unwrap_or(0));
                decide(
                    &CompactionInput {
                        write_tokens: tokens,
                        archive_tokens: cut.archive_tokens,
                        memo_tokens: setup.settings.memo_tokens,
                        ..base_input(&online, &setup.settings, &setup.model, tokens)
                    },
                    &setup.settings.economics,
                )
            }
        };
        let request = SummaryRequest {
            attempt: 1,
            model: setup.model.clone(),
            system: setup.prompt.system.clone(),
            instructions: setup.prompt.instructions.clone(),
            retry: setup.retry,
            cache_key: setup.cache_key.clone(),
            max_tokens: Some(max_summary_tokens(&setup)),
            created_at: rt.now()?,
            tail: cut.tail,
            first_kept: cut.first_kept,
            decision,
        };
        rt.commit_next(running(encode(&Checkpoint::Summarize { request })))
            .await?;
        Ok(())
    }

    // --------------------------------------------------------------- summarize

    async fn summarize(
        &self,
        rt: &TaskRuntime,
        task: &RunningTask,
        request: SummaryRequest,
    ) -> Result<(), TaskError> {
        let conversation = rt.conversation();
        let input = input_of(task)?;
        // `tail` 处的上下文不可变，所以这就是 `select` 选的那一段。
        let view = rt.context(conversation, Some(request.tail)).await?;
        let Some(cut_index) = view
            .entries
            .iter()
            .position(|entry| *entry == request.first_kept)
        else {
            return Err(TaskError::new(format!(
                "the first kept entry {} is not in the context at {}",
                request.first_kept, request.tail
            )));
        };
        let transcript = serialize_conversation(&summarized_messages(&view, cut_index));
        let focus = input
            .instructions
            .as_deref()
            .map(|text| format!("\n\nAdditional focus: {text}"))
            .unwrap_or_default();
        let now = request.created_at;
        let messages = vec![
            Message::System(SystemMessage {
                content: Content::Text(request.system.clone()),
                sections: None,
                tools_added: None,
                tools_removed: None,
                timestamp: now,
                extra: Default::default(),
            }),
            Message::user(
                format!(
                    "<conversation>\n{transcript}\n</conversation>\n\n{}{focus}",
                    request.instructions
                ),
                now,
            ),
        ];
        let model_request =
            ModelRequest::new(&request.model, &messages).with_options(RequestOptions {
                images: None,
                cache_key: request.cache_key.as_deref(),
                max_output_tokens: request.max_tokens,
                revised_from: None,
            });
        let mut progress = |_: &AssistantMessage| {};
        let cancelled = rt.cancelled();
        let result = tokio::select! {
            biased;
            _ = cancelled.cancelled() => {
                // abort 标记或关闭：已提交的状态由 abort handler 或重开后的这个阶段处理。
                return Err(TaskError::new("the compaction was cancelled"));
            }
            result = self.deps.stream.stream(&model_request, &mut progress) => result,
        };
        match result {
            Ok(message) => match summary_text(&message) {
                Some(summary) => self.place(rt, input, request, summary).await,
                None => {
                    let text = summary_failure(&message);
                    self.fail(rt, input.reason, "model_error", text).await
                }
            },
            Err(error) => {
                if retry_cause(&error).is_some() && request.retry.allows_another(request.attempt) {
                    return self.back_off(rt, request, error.to_string()).await;
                }
                self.fail(
                    rt,
                    input.reason,
                    "model_error",
                    format!("Summarization failed: {error}"),
                )
                .await
            }
        }
    }

    /// 写下到点时刻回到 `retry`。
    async fn back_off(
        &self,
        rt: &TaskRuntime,
        request: SummaryRequest,
        error: String,
    ) -> Result<(), TaskError> {
        let conversation = rt.conversation();
        let until = rt.now()? + request.retry.delay_ms(request.attempt) as i64;
        tracing::warn!(attempt = request.attempt, until, %error, "摘要请求失败，持久化退避后再试");
        rt.commit(move |tx, task| {
            Box::pin(async move {
                let mut live = live::load(tx, conversation).await?;
                if let Some(status) = live.compaction_status_mut(task.id()) {
                    status.retry = Some(RetryLive { at: until, error });
                }
                live::store(tx, conversation, &live).await?;
                Ok(Some(running(encode(&Checkpoint::Retry { until, request }))))
            })
        })
        .await?;
        Ok(())
    }

    // -------------------------------------------------------------------- 落点

    /// 放置摘要并结束（一次提交）。阻塞的压缩由它的生成直接追加——生成拿着 run 在等，没有别的
    /// 东西会写这个 conversation；其余的当 write submission 受理，空闲时立刻落（或过期），否则
    /// 在下一个 boundary 落。
    async fn place(
        &self,
        rt: &TaskRuntime,
        input: Input,
        request: SummaryRequest,
        summary: String,
    ) -> Result<(), TaskError> {
        let conversation = rt.conversation();
        let modes = self.modes();
        let now = rt.now()?;
        let reason = input.reason;
        rt.commit(move |tx, task| {
            Box::pin(async move {
                let debt = request.decision.debt();
                let decision = serde_json::to_value(&request.decision).ok();
                let data = compaction_data(reason.as_str(), decision.as_ref(), &debt);
                let model = vec![Message::user(wrap_summary(&summary), now)];
                let blocking = task.record.owner.is_some();
                let result = if blocking {
                    let entry = tx
                        .append_entry(
                            conversation,
                            EntryDraft::new(COMPACTION_ENTRY_KIND)
                                .with_head(DraftHead::At(request.first_kept))
                                .with_model(model)
                                .with_data(data),
                        )
                        .await?;
                    json!({ "entryId": entry.id })
                } else {
                    let draft = SubmissionDraft::write(WriteDraft {
                        kind: COMPACTION_ENTRY_KIND.to_owned(),
                        model: Some(model),
                        data: Some(data),
                        head: Some(WriteHead::At(request.first_kept)),
                        edits: None,
                    })
                    .with_request_id(format!("compaction:{}", task.id()));
                    match inbox::admit(tx, &GenerationDefinition, conversation, &draft, modes)
                        .await?
                    {
                        Admitted::Id(id) => json!({ "submissionId": id }),
                        Admitted::Busy => unreachable!("write 从不因忙被拒"),
                    }
                };
                let mut live = live::load(tx, conversation).await?;
                live.remove_compaction(task.id());
                live.compaction_failure = None;
                live::store(tx, conversation, &live).await?;
                Ok(Some(completed(result)))
            })
        })
        .await?;
        Ok(())
    }

    /// 移除状态、没有摘要地结束（没有可收的）。
    async fn complete(&self, rt: &TaskRuntime, result: Option<Value>) -> Result<(), TaskError> {
        let conversation = rt.conversation();
        rt.commit(move |tx, task| {
            Box::pin(async move {
                let mut live = live::load(tx, conversation).await?;
                live.remove_compaction(task.id());
                live::store(tx, conversation, &live).await?;
                Ok(Some(completed(result.unwrap_or_else(|| json!({})))))
            })
        })
        .await?;
        Ok(())
    }

    /// 失败：移除状态、记下 `compactionFailure`。
    async fn fail(
        &self,
        rt: &TaskRuntime,
        reason: Reason,
        code: &'static str,
        message: String,
    ) -> Result<(), TaskError> {
        let conversation = rt.conversation();
        rt.commit(move |tx, task| {
            Box::pin(async move {
                let mut live = live::load(tx, conversation).await?;
                live.remove_compaction(task.id());
                live.compaction_failure = Some(CompactionFailure {
                    task: task.id(),
                    reason: reason.as_str().to_owned(),
                    error: message.clone(),
                });
                live::store(tx, conversation, &live).await?;
                Ok(Some(failed(message, code, None)))
            })
        })
        .await?;
        Ok(())
    }
}

/// 干净的 `stop`、有文字、没有工具调用：这就是摘要。其他都不是。
fn summary_text(message: &AssistantMessage) -> Option<String> {
    if message.stop_reason != StopReason::Stop || message.tool_calls().next().is_some() {
        return None;
    }
    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn summary_failure(message: &AssistantMessage) -> String {
    match message.stop_reason {
        StopReason::Error | StopReason::Aborted => format!(
            "Summarization failed: {}",
            message
                .extra
                .get("errorMessage")
                .and_then(Value::as_str)
                .map_or_else(|| format!("{:?}", message.stop_reason), str::to_owned)
        ),
        StopReason::Length => {
            "Summarization hit the token limit; the summary is incomplete".to_owned()
        }
        _ if message.tool_calls().next().is_some() => {
            "Summarization attempted to call a tool".to_owned()
        }
        _ => "Summarization produced no text".to_owned(),
    }
}

/// 建一个压缩任务，状态在同一次提交里加进 `komo.live`。`owner` 是在等它的生成（阻塞压缩）；
/// 没有就是 conversation 所有的，除了手动的都是 background。
pub async fn create(
    tx: &mut Tx,
    task: &CompactionTask,
    conversation: ConversationId,
    live: &mut LiveState,
    input: &Input,
    owner: Option<TaskId>,
) -> Result<TaskId, DurableError> {
    let background = owner.is_none() && input.reason != Reason::Manual;
    let id = tx
        .create_task(
            task,
            serde_json::to_value(input).expect("Input 可序列化"),
            owner.map_or(Ownership::Conversation, Ownership::Task),
            TaskOptions {
                conversation: Some(conversation),
                background,
                ..TaskOptions::default()
            },
        )
        .await?;
    let mut statuses = live.compactions.take().unwrap_or_default();
    statuses.push(CompactionStatus {
        task: id,
        reason: input.reason.as_str().to_owned(),
        blocking: owner.is_some(),
        attempt: 1,
        retry: None,
    });
    live.compactions = Some(statuses);
    Ok(id)
}

/// 生成的 `prepare` 读到它等的压缩的结局。`Ok(Some(error))` = 没有摘要的失败原因，`None` = 完成
/// （`entryId` 有没有另说）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// 完成；`summary` = 产出了摘要 entry。
    Done { summary: bool },
    /// 失败、faulted、orphaned 或被直接 abort：文字是"Compaction failed: …"。
    Broke(String),
}

pub fn settled(outcome: &komo_kernel::durable::TaskOutcome) -> Settled {
    use komo_kernel::durable::TaskOutcome;
    match outcome {
        TaskOutcome::Completed { result } => Settled::Done {
            summary: result.get("entryId").is_some(),
        },
        TaskOutcome::Failed { error, .. } | TaskOutcome::Faulted { error } => {
            Settled::Broke(format!("Compaction failed: {}", error.message))
        }
        TaskOutcome::Orphaned { reason } => Settled::Broke(format!("Compaction failed: {reason}")),
        TaskOutcome::Aborted { .. } => Settled::Broke("Compaction was aborted".to_owned()),
    }
}

/// 调度器写的结局（faulted / orphaned）变成 terminal 的那笔提交里的清理（spec §8.7）：移除
/// 状态，记下失败。
pub(in crate::durable::harness) async fn settle_scheduler_outcome(
    tx: &mut Tx,
    conversation: ConversationId,
    task: TaskId,
    reason: &str,
    error: &str,
) -> Result<(), DurableError> {
    let mut live = live::load(tx, conversation).await?;
    let known = live.compaction_status_mut(task).is_some();
    if !known {
        return Ok(());
    }
    let reason = live
        .compaction_status_mut(task)
        .map(|status| status.reason.clone())
        .unwrap_or_else(|| reason.to_owned());
    live.remove_compaction(task);
    live.compaction_failure = Some(CompactionFailure {
        task,
        reason,
        error: error.to_owned(),
    });
    live::store(tx, conversation, &live).await
}

#[async_trait]
impl TaskKind for CompactionTask {
    fn name(&self) -> &str {
        COMPACTION_KIND
    }

    fn initial(&self, _input: &Value) -> Value {
        encode(&Checkpoint::Select)
    }

    async fn run_phase(&self, task: &RunningTask, rt: &TaskRuntime) -> Result<(), TaskError> {
        match decode(task)? {
            Checkpoint::Select => self.select(rt, task).await,
            Checkpoint::Summarize { request } => self.summarize(rt, task, request).await,
            Checkpoint::Retry { until, request } => {
                rt.sleep_until(until).await?;
                let conversation = rt.conversation();
                let attempt = request.attempt + 1;
                rt.commit(move |tx, task| {
                    Box::pin(async move {
                        let mut live = live::load(tx, conversation).await?;
                        if let Some(status) = live.compaction_status_mut(task.id()) {
                            status.attempt = attempt;
                            status.retry = None;
                        }
                        live::store(tx, conversation, &live).await?;
                        let request = SummaryRequest { attempt, ..request };
                        Ok(Some(running(encode(&Checkpoint::Summarize { request }))))
                    })
                })
                .await?;
                Ok(())
            }
        }
    }

    /// 移除状态、以 `aborted` 收场，不写任何 entry；被 abort 或崩溃打断的那次尝试没有已提交的
    /// 回复。
    async fn abort(&self, _task: &RunningTask, rt: &TaskRuntime) -> Result<(), TaskError> {
        let conversation = rt.conversation();
        rt.commit(move |tx, task| {
            Box::pin(async move {
                let mut live = live::load(tx, conversation).await?;
                live.remove_compaction(task.id());
                live::store(tx, conversation, &live).await?;
                Ok(Some(aborted(None)))
            })
        })
        .await?;
        Ok(())
    }
}
