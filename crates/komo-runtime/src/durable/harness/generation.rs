//! `komo.generation`（`docs/storage-durable.md` §4.5、§12.3；pi-durable spec §8.3、§8.5，
//! `harness/generation.ts`）：一轮模型调用。
//!
//! 每个阶段是一个小函数，阶段之间的状态只在 checkpoint 里（**不靠看尾巴猜**）。
//!
//! ```text
//! prepare ─▶ request ─▶ answer                      ─▶ 终态（final boundary，收尾或开后继 run）
//!    ▲           │──▶ tool round ─▶ tools(waiting) ─▶ 终态（postTools boundary，交给下一个 generation）
//!    │           └──▶ error ─▶ retry ──┘            ─▶ 终态（失败，输入 unanswered）
//!    └──────────────────────────────────┘
//! ```
//!
//! checkpoint 的形状（`phase` 是判别，字段 camelCase）：
//!
//! ```json
//! { "phase": "prepare", "attempt": 1 }
//! { "phase": "request", "attempt": 1, "model": {…ModelConfig…}, "cutoff": 12,
//!   "limits": { "retry": {"maxAttempts":10,"baseMs":2000,"maxWaitMs":60000}, "maxTokens": null },
//!   "cacheKey": "…", "maxOutputTokens": null }
//! { "phase": "retry", "attempt": 2, "until": 1761206400000 }
//! { "phase": "tools", "assistant": 13, "tools": [14, 15], "pending": ["call_3"] }
//! ```
//!
//! - `prepare` = `prepareNextTurn` + `prepareRequest`：问钩子拿系统段、工具表、模型与预算，
//!   变了才写 `komo.system` entry，再把冻结的模型、预算与 `cutoff`（这次请求包含的最新
//!   entry）写进 `request`。**重试回到这里**，所以配置与设置的变化在下一次尝试生效。
//! - `request` = `stream`：无状态地把 `cutoff` 之前的上下文发给模型。**重启后回到这里**：
//!   已节流提交的部分回复先转成一条 `aborted` 的 assistant entry，再按已提交上下文原样重发。
//! - `retry`：持久化的次数与到点时刻（`until`）；预算 2 s 起、封顶 60 s、10 次，
//!   成功一轮就清零（下一个 generation 从 attempt 1 起）。
//! - `tools`：等这一轮的工具任务。工具按"组"建：连续的只读调用同时在飞，有副作用的与
//!   codemode 自成一组、一个一个来；`pending` 是还没建的调用。一轮结束在 postTools
//!   boundary 把 run 交给下一个 generation，或在 final boundary 收尾。
//!
//! - **压缩**（`docs/storage-durable.md` §4.5；pi spec §8.3 / §8.7）：`prepare` 在装配之后调
//!   `compaction::plan`。贴着窗口——先换短视图（一条只带 `edits` 的 `komo.decay` write entry，
//!   和 `komo.system` 一起在同一次提交里追加）；没有可换的就建一个**阻塞压缩**（生成拥有的
//!   子任务）并 `waiting`，checkpoint `{ phase: "prepare", compacted, threshold: true }`，压缩
//!   完回到 `prepare` 读一次它的结局再接着这一轮。到了后台阈值只在移到 `request` 的那次提交里
//!   多建一个 conversation 所有的后台压缩，不等它。
//! - **超长**：`request` 收到 provider 的上下文超长错误（`is_context_overflow`）、这个生成还没
//!   压过（`compacted` 空）、旧实现的"压成之后至少有一次成功请求"也满足时，追加错误 entry、建
//!   阻塞压缩（`reason: overflow`，决策记 `context_overflow: true`）并 `waiting`，checkpoint
//!   `{ phase: "prepare", compacted, overflow }`；这次恢复请求不算重试预算。压缩没产出摘要、或
//!   压过之后又超长，都照旧以 `model_error` 失败——不会无限循环。
//!
//! 预算（轮数、token）落在 `komo.live.run`，重启不重置。被截断的回复（`length`）不按半轮调用
//! 执行：它记成一条失败的 assistant entry，run 以 `model_error` 收场（沿用今天的规则）。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::compaction::PROMPT_CACHE_TTL;
use komo_kernel::durable::compaction::{DECAY_ENTRY_KIND, cache_cold, decay_edits, online_state};
use komo_kernel::durable::task::RunningTask;
use komo_kernel::durable::{
    AssistantMessage, ContentBlock, ConversationId, EntryId, EntryRecord, Message, StopReason,
    SubmissionSettlement, TaskId, derive_context,
};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::turn::LlmError;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::compaction::{self, CompactionTask, Plan, PlanInput, Reason, Settled};
use super::entries::append_assistant;
use super::hooks::{HarnessDeps, HookError, RetryPolicy, ToolSet};
use super::inbox::{self, At, QueueModes};
use super::live::{self, GenerationLive, RetryLive, ToolSlot};
use super::outcome::{aborted, completed, failed, running, unanswered, waiting};
use super::system::plan_system_entries;
use super::tool::ToolTask;
use super::{GENERATION_KIND, entries};
use crate::durable::error::{DurableError, TaskError};
use crate::durable::kind::TaskKind;
use crate::durable::runtime::TaskRuntime;
use crate::durable::tx::{EntryDraft, Ownership, TaskOptions};
use crate::llm::{ModelRequest, RequestOptions, is_context_overflow, retry_cause, usage_of};

/// 被截断的回复失败时的文字（沿用 `AgentLoop` 的）。
const TRUNCATED: &str = "模型回复被截断，调用参数未收齐——不按半轮调用执行";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub retry: RetryPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "phase",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Checkpoint {
    Prepare {
        attempt: u32,
        /// 这个生成等过的阻塞压缩：之后不再起别的压缩（要么这一轮已经压过，要么压不动）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        /// 起 `compacted` 的那次超长的错误文字；`prepare` 恢复时查一次它有没有产出摘要。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overflow: Option<String>,
        /// `compacted` 是越过阻塞阈值起的；`prepare` 恢复时查一次它的结局。
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        threshold: bool,
    },
    Request {
        attempt: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        /// 这一轮刚把 `messages[i]` 起的工具结果换了短视图（衰减）：Anthropic 的缓存断点要补在
        /// 它之前（`RequestOptions::revised_from`）。重启后重发时带同一个值。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revised_from: Option<usize>,
        /// 这次请求冻结的模型与 effort（配置热重载不换正在发的这一个）。
        model: ModelConfig,
        /// 这次请求包含的最新 entry。
        cutoff: EntryId,
        limits: Limits,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_output_tokens: Option<u32>,
    },
    Retry {
        attempt: u32,
        /// 到点时刻（毫秒）。
        until: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
    },
    Tools {
        /// 发起这一轮调用的 assistant entry。
        assistant: EntryId,
        /// 到目前为止建的工具任务，按调用顺序。
        tools: Vec<TaskId>,
        /// 还没建的调用（顺序组），按调用顺序。
        pending: Vec<String>,
    },
}

fn encode(checkpoint: &Checkpoint) -> Value {
    serde_json::to_value(checkpoint).expect("Checkpoint 可序列化")
}

impl Checkpoint {
    fn prepare(attempt: u32, compacted: Option<TaskId>) -> Self {
        Checkpoint::Prepare {
            attempt,
            compacted,
            overflow: None,
            threshold: false,
        }
    }
}

/// 只有定义没有行为的生成任务：`inbox` / `compaction` 在一次提交里建生成任务时只要它的名字、
/// 版本与初始 checkpoint（`Tx::create_task` 的 `&dyn TaskKind`），真正跑它的是注册表里的
/// [`GenerationTask`]。
pub(super) struct GenerationDefinition;

#[async_trait]
impl TaskKind for GenerationDefinition {
    fn name(&self) -> &str {
        GENERATION_KIND
    }

    fn initial(&self, _input: &Value) -> Value {
        encode(&Checkpoint::prepare(1, None))
    }

    async fn run_phase(&self, _task: &RunningTask, _rt: &TaskRuntime) -> Result<(), TaskError> {
        Err(TaskError::new(
            "GenerationDefinition only describes the task",
        ))
    }

    async fn abort(&self, _task: &RunningTask, _rt: &TaskRuntime) -> Result<(), TaskError> {
        Err(TaskError::new(
            "GenerationDefinition only describes the task",
        ))
    }
}

fn decode(task: &RunningTask) -> Result<Checkpoint, TaskError> {
    serde_json::from_value(task.checkpoint.clone())
        .map_err(|e| TaskError::new(format!("generation checkpoint is malformed: {e}")))
}

/// 生成任务。克隆便宜（共享同一份依赖），闭包里要建任务时克隆一份进去。
#[derive(Clone)]
pub struct GenerationTask {
    deps: Arc<HarnessDeps>,
    tool: ToolTask,
    compaction: CompactionTask,
}

impl GenerationTask {
    pub fn new(deps: Arc<HarnessDeps>, tool: ToolTask) -> Self {
        let compaction = CompactionTask::new(deps.clone());
        Self {
            deps,
            tool,
            compaction,
        }
    }

    /// 它建的压缩任务的定义：`Harness::install` 一并装进注册表。
    pub fn compaction(&self) -> &CompactionTask {
        &self.compaction
    }

    pub fn deps(&self) -> &Arc<HarnessDeps> {
        &self.deps
    }

    fn modes(&self) -> QueueModes {
        let settings = self.deps.settings();
        QueueModes {
            steering: settings.steering_mode,
            follow_up: settings.follow_up_mode,
        }
    }

    // ------------------------------------------------------------------ prepare

    async fn prepare(
        &self,
        rt: &TaskRuntime,
        attempt: u32,
        resumed: Resumed,
    ) -> Result<(), TaskError> {
        let conversation = rt.conversation();
        let Resumed {
            compacted,
            overflow,
            threshold,
        } = resumed;
        // 阻塞压缩回来了：读一次它的结局（spec §8.3）。有摘要，或阈值压缩"没有可收的"，接着
        // 这一轮；超长救不回来、压缩失败 / 被 abort 都以 `model_error` 收场，不发请求。
        if let Some(task) = compacted
            && (overflow.is_some() || threshold)
        {
            let outcome = rt
                .outcomes(&[task])
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| TaskError::new("the awaited compaction has no outcome"))?;
            let settled = compaction::task::settled(&outcome);
            let proceed = match &settled {
                Settled::Done { summary } => *summary || overflow.is_none(),
                Settled::Broke(_) => false,
            };
            if !proceed {
                let text = overflow.unwrap_or_else(|| match settled {
                    Settled::Broke(text) => text,
                    Settled::Done { .. } => "Compaction produced no summary".to_owned(),
                });
                return self.fail_run(rt, "model_error", text, None).await;
            }
        }

        let entries = rt.entries(conversation, None).await?;
        let view = derive_context(&entries, None);
        let setup = match self.deps.hooks.setup(conversation, &view.messages).await {
            Ok(setup) => setup,
            Err(HookError::NoModel(message)) => {
                return self.fail_run(rt, "no_model", message, None).await;
            }
            Err(HookError::Failed(message)) => return Err(TaskError::new(message)),
        };
        let mut drafts = plan_system_entries(
            &entries,
            &view,
            &setup.sections,
            &setup.tools.definitions(),
            rt.now()?,
        );

        // 压缩判断（spec §8.3 / §8.7）：这个生成还没等过压缩才判。
        let mut revised_from = None;
        let mut background = None;
        if compacted.is_none() {
            let csetup = match self.deps.hooks.compaction(conversation).await {
                Ok(csetup) => csetup,
                Err(HookError::NoModel(message)) => {
                    return self.fail_run(rt, "no_model", message, None).await;
                }
                Err(HookError::Failed(message)) => return Err(TaskError::new(message)),
            };
            let live = compaction::read_live(rt, conversation).await?;
            let online = online_state(&entries, compaction::refused_after(&live));
            let cold = cache_cold(&entries, rt.now()?, PROMPT_CACHE_TTL.as_millis() as i64);
            let planned: Vec<Message> = drafts
                .iter()
                .flat_map(|draft| draft.model.iter().flatten().cloned())
                .collect();
            let plan = compaction::plan(PlanInput {
                entries: &entries,
                view: &view,
                planned: &planned,
                online: &online,
                setup: &csetup,
                model: &setup.model,
                decay: setup.tools.env.projection.decay,
                cache_cold: cold,
                compaction_running: live.compactions.as_ref().is_some_and(|c| !c.is_empty()),
            });
            match plan {
                Plan::Nothing => {}
                Plan::Compact { decision } => {
                    return self.wait_for_compaction(rt, attempt, decision).await;
                }
                Plan::Background { decision } => background = Some(decision),
                Plan::Decay {
                    candidates,
                    decision,
                } => {
                    let mut draft = EntryDraft::new(DECAY_ENTRY_KIND);
                    draft.edits = decay_edits(&candidates);
                    draft.data = Some(json!({
                        "decayed": candidates.iter().map(|c| c.entry).collect::<Vec<_>>(),
                        "debt": decision.debt(),
                        "decision": decision,
                    }));
                    drafts.insert(0, draft);
                    revised_from = first_revised(conversation, &entries, &view, &drafts);
                }
            }
        }

        let newest = entries.last().map(|entry| entry.id);
        let request = Checkpoint::Request {
            attempt,
            compacted,
            revised_from,
            model: setup.model.clone(),
            // 提交里会换成最后一条 system entry 的 ID。
            cutoff: newest.unwrap_or(EntryId::new(0)),
            limits: Limits {
                retry: setup.budget.retry,
                max_tokens: setup.budget.max_tokens,
            },
            cache_key: setup.cache_key.clone(),
            max_output_tokens: setup.max_output_tokens,
        };
        let max_rounds = setup.budget.max_rounds;
        let compaction_task = self.compaction.clone();
        rt.commit(move |tx, task| {
            Box::pin(async move {
                let mut live = live::load(tx, conversation).await?;
                // 预算：重启不重置，计数在 `komo.live.run`。
                if let (Some(max), Some(run)) = (max_rounds, live.run.as_ref())
                    && run.task == task.id()
                    && run.rounds >= max
                {
                    let message = format!("超过总轮数预算（{max}）");
                    live::end_run(
                        tx,
                        &mut live,
                        task.id(),
                        &unanswered("budget", Some(&message)),
                    )
                    .await?;
                    live::store(tx, conversation, &live).await?;
                    return Ok(Some(failed(message, "budget", None)));
                }
                let mut cutoff = newest;
                for draft in drafts {
                    cutoff = Some(tx.append_entry(conversation, draft).await?.id);
                }
                let Some(cutoff) = cutoff else {
                    return Err(DurableError::rejected(format!(
                        "Conversation {conversation} has no entries to send"
                    )));
                };
                // 后台压缩：不等它，只在这次提交里把它建出来（它自己落点，见 `compaction::task`）。
                if let Some(decision) = background
                    && live.compactions.as_ref().is_none_or(Vec::is_empty)
                {
                    compaction::task::create(
                        tx,
                        &compaction_task,
                        conversation,
                        &mut live,
                        &compaction::Input::new(Reason::Threshold, Some(decision)),
                        None,
                    )
                    .await?;
                    live::store(tx, conversation, &live).await?;
                }
                let mut request = request;
                if let Checkpoint::Request { cutoff: slot, .. } = &mut request {
                    *slot = cutoff;
                }
                Ok(Some(running(encode(&request))))
            })
        })
        .await?;
        Ok(())
    }

    /// 越过阻塞阈值：建一个生成拥有的压缩并 `waiting`（spec §8.3）。
    async fn wait_for_compaction(
        &self,
        rt: &TaskRuntime,
        attempt: u32,
        decision: komo_kernel::compaction::CompactionDecision,
    ) -> Result<(), TaskError> {
        let conversation = rt.conversation();
        let compaction_task = self.compaction.clone();
        rt.commit(move |tx, task| {
            Box::pin(async move {
                let mut live = live::load(tx, conversation).await?;
                let id = compaction::task::create(
                    tx,
                    &compaction_task,
                    conversation,
                    &mut live,
                    &compaction::Input::new(Reason::Threshold, Some(decision)),
                    Some(task.id()),
                )
                .await?;
                live::store(tx, conversation, &live).await?;
                let checkpoint = Checkpoint::Prepare {
                    attempt,
                    compacted: Some(id),
                    overflow: None,
                    threshold: true,
                };
                Ok(Some(waiting(encode(&checkpoint), vec![id])))
            })
        })
        .await?;
        Ok(())
    }

    /// 上下文超长：能救就追加错误 entry、建阻塞压缩并 `waiting`，返回 `true`；救不了
    /// （这个生成已经压过、刚压成摘要还没有成功的请求、切不出可收的一段）返回 `false`，调用方
    /// 照旧失败。这次恢复请求不算重试预算：`attempt` 原样带回 `prepare`。
    async fn start_overflow_compaction(
        &self,
        rt: &TaskRuntime,
        request: &RequestCheckpoint,
        message: &AssistantMessage,
        text: &str,
    ) -> Result<bool, TaskError> {
        if request.compacted.is_some() {
            return Ok(false);
        }
        let conversation = rt.conversation();
        let csetup = match self.deps.hooks.compaction(conversation).await {
            Ok(csetup) => csetup,
            // 拿不到设置就救不了：照旧按超长失败。
            Err(_) => return Ok(false),
        };
        let entries = rt.entries(conversation, None).await?;
        let view = derive_context(&entries, None);
        let online = online_state(&entries, None);
        let cold = cache_cold(&entries, rt.now()?, PROMPT_CACHE_TTL.as_millis() as i64);
        let Some((decision, _)) =
            compaction::plan_overflow(&view, &online, &csetup.settings, &request.model, cold)
        else {
            return Ok(false);
        };
        tracing::warn!(attempt = request.attempt, "上下文超长，压缩一次后重发");
        let attempt = request.attempt;
        let message = message.clone();
        let text = text.to_owned();
        let compaction_task = self.compaction.clone();
        rt.commit(move |tx, task| {
            Box::pin(async move {
                let mut live = live::load(tx, conversation).await?;
                append_assistant(tx, conversation, message).await?;
                live.generation = None;
                let id = compaction::task::create(
                    tx,
                    &compaction_task,
                    conversation,
                    &mut live,
                    &compaction::Input::new(Reason::Overflow, Some(decision)),
                    Some(task.id()),
                )
                .await?;
                live::store(tx, conversation, &live).await?;
                let checkpoint = Checkpoint::Prepare {
                    attempt,
                    compacted: Some(id),
                    overflow: Some(text),
                    threshold: false,
                };
                Ok(Some(waiting(encode(&checkpoint), vec![id])))
            })
        })
        .await?;
        Ok(true)
    }

    // ------------------------------------------------------------------ request

    async fn request(&self, rt: &TaskRuntime, request: RequestCheckpoint) -> Result<(), TaskError> {
        let conversation = rt.conversation();
        let attempt = request.attempt;
        // 重启回到这里：上一次中断留下的部分回复转成 aborted entry，再重发。
        rt.commit(move |tx, _| {
            Box::pin(async move {
                let mut live = live::load(tx, conversation).await?;
                live::convert_partial(tx, conversation, &mut live).await?;
                live.generation = Some(GenerationLive {
                    attempt,
                    message: None,
                    retry: None,
                });
                live::store(tx, conversation, &live).await?;
                Ok(None)
            })
        })
        .await?;

        let tools = match self.deps.hooks.tools(conversation).await {
            Ok(tools) => tools,
            Err(HookError::NoModel(message)) => {
                return self.fail_run(rt, "no_model", message, None).await;
            }
            Err(HookError::Failed(message)) => return Err(TaskError::new(message)),
        };
        let view = rt.context(conversation, Some(request.cutoff)).await?;
        let result = self.stream(rt, &request, &view.messages, &tools).await?;
        // 取消 / 关闭：committed 状态由 abort 或重开后的这个阶段处理。
        let message = match result {
            Ok(message) => message,
            Err(error) => return self.classify_error(rt, &request, error).await,
        };
        self.classify(rt, &request, message, &tools).await
    }

    /// 流一次请求，部分回复最多每 `partial_interval` 提交一次；返回前停掉节流并等在飞的提交
    /// 落地，所以不会有过期的部分回复落在结局之后。取消赢了就是 `Err`。
    async fn stream(
        &self,
        rt: &TaskRuntime,
        request: &RequestCheckpoint,
        messages: &[Message],
        tools: &ToolSet,
    ) -> Result<Result<AssistantMessage, LlmError>, TaskError> {
        let interval = self.deps.settings().partial_interval;
        let latest: Arc<Mutex<Option<AssistantMessage>>> = Arc::default();
        let stop = CancellationToken::new();
        let committer = tokio::spawn(commit_partials(
            rt.clone(),
            latest.clone(),
            stop.clone(),
            interval,
            request.attempt,
        ));
        let model_request =
            ModelRequest::new(&request.model, messages).with_options(RequestOptions {
                images: tools.env.images.as_deref(),
                cache_key: request.cache_key.as_deref(),
                max_output_tokens: request.max_output_tokens,
                revised_from: request.revised_from,
            });
        let sink = latest.clone();
        let mut progress = move |partial: &AssistantMessage| {
            // 开头没有内容的部分回复（provider 的 `start` 帧）什么都不展示。
            if !partial.content.is_empty() {
                *sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(partial.clone());
            }
        };
        let cancelled = rt.cancelled();
        let result = tokio::select! {
            biased;
            _ = cancelled.cancelled() => None,
            result = self.deps.stream.stream(&model_request, &mut progress) => Some(result),
        };
        stop.cancel();
        let _ = committer.await;
        result.ok_or_else(|| TaskError::new("the generation was cancelled"))
    }

    /// 终态回复分类（一次提交，同时清掉部分回复）。
    async fn classify(
        &self,
        rt: &TaskRuntime,
        request: &RequestCheckpoint,
        message: AssistantMessage,
        tools: &ToolSet,
    ) -> Result<(), TaskError> {
        match message.stop_reason {
            StopReason::ToolUse | StopReason::Stop => {
                let calls = entries_tool_calls(&message);
                if message.stop_reason == StopReason::ToolUse && !calls.is_empty() {
                    self.start_tool_round(rt, request, message, calls, tools)
                        .await
                } else {
                    self.answer(rt, message).await
                }
            }
            StopReason::Length => {
                // 截断的一轮：不按半轮调用执行，连这一条也不进之后的上下文。
                let mut message = message;
                message.stop_reason = StopReason::Error;
                message
                    .extra
                    .insert("errorMessage".into(), json!(TRUNCATED));
                self.fail_with_message(rt, message, TRUNCATED.to_owned())
                    .await
            }
            StopReason::Error | StopReason::Aborted | StopReason::Deferred => {
                let text = message
                    .extra
                    .get("errorMessage")
                    .and_then(Value::as_str)
                    .map_or_else(
                        || {
                            format!(
                                "Model response ended with stop reason {:?}",
                                message.stop_reason
                            )
                        },
                        str::to_owned,
                    );
                self.fail_with_message(rt, message, text).await
            }
        }
    }

    /// `ModelStream` 报错：能退避的写下到点时刻回到 `prepare`，不能的（或预算用完）失败。
    async fn classify_error(
        &self,
        rt: &TaskRuntime,
        request: &RequestCheckpoint,
        error: LlmError,
    ) -> Result<(), TaskError> {
        let text = error.to_string();
        let message = error_message(&request.model, &text, rt.now()?);
        if is_context_overflow(&error)
            && self
                .start_overflow_compaction(rt, request, &message, &text)
                .await?
        {
            return Ok(());
        }
        let retryable = retry_cause(&error).is_some();
        if retryable && request.limits.retry.allows_another(request.attempt) {
            let conversation = rt.conversation();
            let attempt = request.attempt;
            let compacted = request.compacted;
            let until = rt.now()? + request.limits.retry.delay_ms(attempt) as i64;
            tracing::warn!(attempt, until, %error, "模型请求失败，持久化退避后再试");
            return rt
                .commit(move |tx, _| {
                    Box::pin(async move {
                        let mut live = live::load(tx, conversation).await?;
                        append_assistant(tx, conversation, message).await?;
                        live.generation = Some(GenerationLive {
                            attempt,
                            message: None,
                            retry: Some(RetryLive {
                                at: until,
                                error: text,
                            }),
                        });
                        live::store(tx, conversation, &live).await?;
                        Ok(Some(running(encode(&Checkpoint::Retry {
                            attempt,
                            until,
                            compacted,
                        }))))
                    })
                })
                .await
                .map_err(Into::into);
        }
        let reason = if retryable {
            format!(
                "重试预算已用完（{}/{}）：{text}",
                request.attempt, request.limits.retry.max_attempts
            )
        } else {
            text
        };
        self.fail_with_message(rt, message, reason).await
    }

    /// 追加失败的 assistant entry，输入以 `model_error` 收场，任务失败。
    async fn fail_with_message(
        &self,
        rt: &TaskRuntime,
        message: AssistantMessage,
        text: String,
    ) -> Result<(), TaskError> {
        let conversation = rt.conversation();
        rt.commit(move |tx, task| {
            Box::pin(async move {
                let mut live = live::load(tx, conversation).await?;
                append_assistant(tx, conversation, message).await?;
                live::end_run(
                    tx,
                    &mut live,
                    task.id(),
                    &unanswered("model_error", Some(&text)),
                )
                .await?;
                live::store(tx, conversation, &live).await?;
                Ok(Some(failed(text, "model_error", None)))
            })
        })
        .await?;
        Ok(())
    }

    /// 没有模型 / 预算之类在请求之前就定下来的失败：输入 `unanswered`，任务失败。
    async fn fail_run(
        &self,
        rt: &TaskRuntime,
        reason: &'static str,
        message: String,
        detail: Option<String>,
    ) -> Result<(), TaskError> {
        let conversation = rt.conversation();
        rt.commit(move |tx, task| {
            Box::pin(async move {
                let mut live = live::load(tx, conversation).await?;
                live::convert_partial(tx, conversation, &mut live).await?;
                let text = detail.unwrap_or_else(|| message.clone());
                live::end_run(tx, &mut live, task.id(), &unanswered(reason, Some(&text))).await?;
                live::store(tx, conversation, &live).await?;
                Ok(Some(failed(message, reason, None)))
            })
        })
        .await?;
        Ok(())
    }

    /// 最终回答：final boundary 放收件箱里排着的项；输入 `done`，选中的用户项开后继 run。
    async fn answer(&self, rt: &TaskRuntime, message: AssistantMessage) -> Result<(), TaskError> {
        let conversation = rt.conversation();
        let modes = self.modes();
        let this = self.clone();
        rt.commit(move |tx, task| {
            Box::pin(async move {
                // 表读先于第一次表写。
                let mut boundary = inbox::prepare_boundary(tx, conversation, modes).await?;
                let mut live = live::load(tx, conversation).await?;
                let now = tx.now();
                let entry = append_assistant(tx, conversation, message).await?;
                let result = inbox::apply_boundary(tx, &mut boundary, At::Final, now).await?;
                live::end_run(
                    tx,
                    &mut live,
                    task.id(),
                    &SubmissionSettlement::Done { answer: entry.id },
                )
                .await?;
                if !result.users.is_empty() {
                    inbox::start_run(tx, &this, conversation, &mut live, result.users).await?;
                }
                live::store(tx, conversation, &live).await?;
                Ok(Some(completed(json!({ "entryId": entry.id }))))
            })
        })
        .await?;
        Ok(())
    }

    /// 追加发起调用的 assistant entry，同一次提交里建第一组工具任务并 `waiting`。
    async fn start_tool_round(
        &self,
        rt: &TaskRuntime,
        request: &RequestCheckpoint,
        message: AssistantMessage,
        calls: Vec<(String, String)>,
        tools: &ToolSet,
    ) -> Result<(), TaskError> {
        let conversation = rt.conversation();
        let size = group_len(tools, &calls);
        let spent = crate::agent::spent(&usage_of(&message));
        let max_tokens = request.limits.max_tokens;
        let this = self.clone();
        rt.commit(move |tx, task| {
            Box::pin(async move {
                let mut live = live::load(tx, conversation).await?;
                let entry = append_assistant(tx, conversation, message).await?;
                // 预算：先记这一轮，再看 token 有没有超（回复已落账，调用不执行）。
                if let Some(run) = live.run.as_mut()
                    && run.task == task.id()
                {
                    run.rounds += 1;
                    run.tokens += spent;
                    if let Some(limit) = max_tokens
                        && run.tokens > limit
                    {
                        let text = format!("超过 token 预算（{limit}）");
                        live::end_run(tx, &mut live, task.id(), &unanswered("budget", Some(&text)))
                            .await?;
                        live::store(tx, conversation, &live).await?;
                        return Ok(Some(failed(text, "budget", None)));
                    }
                }
                let (group, rest) = calls.split_at(size);
                let ids = create_group(tx, &this, task.id(), entry.id, group).await?;
                let mut slots: Vec<ToolSlot> = Vec::with_capacity(calls.len());
                for (index, (call_id, name)) in calls.iter().enumerate() {
                    let mut slot = ToolSlot::pending(call_id, name);
                    slot.task_id = ids.get(index).copied();
                    slots.push(slot);
                }
                live.generation = None;
                live.tools = Some(slots);
                live::store(tx, conversation, &live).await?;
                let checkpoint = Checkpoint::Tools {
                    assistant: entry.id,
                    tools: ids.clone(),
                    pending: rest.iter().map(|(id, _)| id.clone()).collect(),
                };
                Ok(Some(waiting(encode(&checkpoint), ids)))
            })
        })
        .await?;
        Ok(())
    }

    // ------------------------------------------------------------------ tools

    async fn tools_phase(
        &self,
        rt: &TaskRuntime,
        assistant: EntryId,
        tools: Vec<TaskId>,
        pending: Vec<String>,
    ) -> Result<(), TaskError> {
        if pending.is_empty() {
            return self.finish_round(rt, assistant).await;
        }
        // 顺序组：建下一组调用的工具任务，再等它们。
        let conversation = rt.conversation();
        let calls = self.pending_calls(rt, assistant, &pending).await?;
        let set = match self.deps.hooks.tools(conversation).await {
            Ok(set) => set,
            Err(HookError::NoModel(message)) | Err(HookError::Failed(message)) => {
                return Err(TaskError::new(message));
            }
        };
        let size = group_len(&set, &calls);
        let this = self.clone();
        rt.commit(move |tx, task| {
            Box::pin(async move {
                let mut live = live::load(tx, conversation).await?;
                let (group, rest) = calls.split_at(size);
                let ids = create_group(tx, &this, task.id(), assistant, group).await?;
                for ((call_id, _), id) in group.iter().zip(&ids) {
                    if let Some(slot) = live
                        .tools
                        .iter_mut()
                        .flatten()
                        .find(|slot| &slot.call_id == call_id && slot.task_id.is_none())
                    {
                        slot.task_id = Some(*id);
                    }
                }
                live::store(tx, conversation, &live).await?;
                let mut all = tools;
                all.extend(&ids);
                let checkpoint = Checkpoint::Tools {
                    assistant,
                    tools: all,
                    pending: rest.iter().map(|(id, _)| id.clone()).collect(),
                };
                Ok(Some(waiting(encode(&checkpoint), ids)))
            })
        })
        .await?;
        Ok(())
    }

    /// `pending` 里的调用（id 与名字），按调用顺序，从发起它们的 assistant entry 读。
    async fn pending_calls(
        &self,
        rt: &TaskRuntime,
        assistant: EntryId,
        pending: &[String],
    ) -> Result<Vec<(String, String)>, TaskError> {
        let entry = rt
            .entry(assistant)
            .await?
            .ok_or_else(|| TaskError::new(format!("entry {assistant} does not exist")))?;
        let calls = entry
            .model
            .iter()
            .flatten()
            .find_map(|message| match message {
                Message::Assistant(message) => Some(entries_tool_calls(message)),
                _ => None,
            })
            .unwrap_or_default();
        Ok(pending
            .iter()
            .filter_map(|id| calls.iter().find(|(call, _)| call == id).cloned())
            .collect())
    }

    /// 这一轮的工具都终结了：在 postTools boundary 把 run 交给下一个 generation；选中的 reset
    /// 把它变成 final，当前 run 以 `reset` 收场（它的上下文在回答之前被切掉了）。
    async fn finish_round(&self, rt: &TaskRuntime, assistant: EntryId) -> Result<(), TaskError> {
        let conversation = rt.conversation();
        let modes = self.modes();
        let this = self.clone();
        rt.commit(move |tx, task| {
            Box::pin(async move {
                let mut boundary = inbox::prepare_boundary(tx, conversation, modes).await?;
                let mut live = live::load(tx, conversation).await?;
                let now = tx.now();
                let result = inbox::apply_boundary(tx, &mut boundary, At::PostTools, now).await?;
                if result.reset {
                    live::end_run(tx, &mut live, task.id(), &unanswered("reset", None)).await?;
                    if !result.users.is_empty() {
                        inbox::start_run(tx, &this, conversation, &mut live, result.users).await?;
                    }
                } else {
                    live.tools = None;
                    let next = inbox::create_generation(tx, &this, conversation).await?;
                    if let Some(run) = live.run.as_mut()
                        && run.task == task.id()
                    {
                        run.inputs.extend(result.users);
                        run.task = next;
                    }
                }
                live::store(tx, conversation, &live).await?;
                Ok(Some(completed(json!({ "entryId": assistant }))))
            })
        })
        .await?;
        Ok(())
    }
}

/// `prepare` 是从什么恢复的：等过的阻塞压缩与它的起因。
struct Resumed {
    compacted: Option<TaskId>,
    overflow: Option<String>,
    threshold: bool,
}

/// 追加 `drafts` 之后，请求里第一条被换了视图的工具结果的下标（Anthropic 缓存断点要补在它
/// 之前）。追加的 entry 在模拟里按顺序编号，推导出来的消息与现在的逐条比。
fn first_revised(
    conversation: ConversationId,
    entries: &[EntryRecord],
    view: &komo_kernel::durable::ContextView,
    drafts: &[EntryDraft],
) -> Option<usize> {
    let mut simulated = entries.to_vec();
    let mut next = entries.last().map_or(0, |entry| entry.id.get());
    for draft in drafts {
        next += 1;
        let mut record = EntryRecord::new(EntryId::new(next), conversation, draft.kind.clone());
        record.model = draft.model.clone();
        record.edits = draft.edits.clone();
        simulated.push(record);
    }
    let after = derive_context(&simulated, None).messages;
    after
        .iter()
        .zip(&view.messages)
        .position(|(new, old)| matches!(new, Message::ToolResult(_)) && new != old)
}

/// 这条消息里的工具调用 `(id, name)`，按调用顺序。
fn entries_tool_calls(message: &AssistantMessage) -> Vec<(String, String)> {
    message
        .tool_calls()
        .map(|(id, name)| (id.to_owned(), name.to_owned()))
        .collect()
}

/// 下一组有几个调用：第一个是只读的，就连同后面连续的只读调用一起（同时在飞）；否则它自成
/// 一组。
fn group_len(tools: &ToolSet, calls: &[(String, String)]) -> usize {
    let Some((_, first)) = calls.first() else {
        return 0;
    };
    if !tools.parallel_safe(first) {
        return 1;
    }
    calls
        .iter()
        .take_while(|(_, name)| tools.parallel_safe(name))
        .count()
}

/// 建一组工具任务，由生成任务拥有；返回任务 ID，按调用顺序。
async fn create_group(
    tx: &mut crate::durable::tx::Tx,
    generation: &GenerationTask,
    owner: TaskId,
    assistant: EntryId,
    group: &[(String, String)],
) -> Result<Vec<TaskId>, DurableError> {
    let mut ids = Vec::with_capacity(group.len());
    for (call_id, _) in group {
        let id = tx
            .create_task(
                &generation.tool,
                json!({ "kind": "model", "assistant": assistant, "callId": call_id }),
                Ownership::Task(owner),
                TaskOptions::default(),
            )
            .await?;
        ids.push(id);
    }
    Ok(ids)
}

/// 失败的请求记成一条 assistant entry：没有内容，`stopReason: error`，错误文字在 `errorMessage`。
fn error_message(model: &ModelConfig, text: &str, now: i64) -> AssistantMessage {
    let mut extra = Map::new();
    extra.insert("provider".into(), json!(model.provider));
    extra.insert("model".into(), json!(model.model));
    extra.insert("errorMessage".into(), json!(text));
    AssistantMessage {
        content: Vec::<ContentBlock>::new(),
        stop_reason: StopReason::Error,
        timestamp: now,
        extra,
    }
}

/// 节流提交部分回复（§3.6）：每 `interval` 最多一次、同一时刻一次在飞，只写 `komo.live` 的
/// 一个字段，所以经 `Line` 走 `commit_progress`。`stop` 之后返回；在飞的那次先完成。
async fn commit_partials(
    rt: TaskRuntime,
    latest: Arc<Mutex<Option<AssistantMessage>>>,
    stop: CancellationToken,
    interval: std::time::Duration,
    attempt: u32,
) {
    let conversation = rt.conversation();
    let mut written: Option<Value> = None;
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }
        let Some(partial) = latest.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            continue;
        };
        let Ok(value) = serde_json::to_value(&partial) else {
            continue;
        };
        if written.as_ref() == Some(&value) {
            continue;
        }
        let stored = value.clone();
        let committed = rt
            .commit(move |tx, _| {
                Box::pin(async move {
                    tx.mark_progress();
                    let mut live = live::load(tx, conversation).await?;
                    let generation = live.generation.get_or_insert(GenerationLive {
                        attempt,
                        message: None,
                        retry: None,
                    });
                    generation.message = Some(stored);
                    live::store(tx, conversation, &live).await?;
                    Ok(None)
                })
            })
            .await;
        match committed {
            Ok(()) => written = Some(value),
            // abort 标记或关闭之后的拒绝是预期的；已提交状态保持一致。
            Err(_) if rt.cancelled().is_cancelled() => return,
            Err(error) => {
                let _ = rt.report(&format!("committing a partial reply failed: {error}"));
            }
        }
    }
}

/// `request` 阶段 checkpoint 的字段（解出来给各函数传）。
#[derive(Debug, Clone)]
pub(super) struct RequestCheckpoint {
    attempt: u32,
    compacted: Option<TaskId>,
    revised_from: Option<usize>,
    model: ModelConfig,
    cutoff: EntryId,
    limits: Limits,
    cache_key: Option<String>,
    max_output_tokens: Option<u32>,
}

#[async_trait]
impl TaskKind for GenerationTask {
    fn name(&self) -> &str {
        GENERATION_KIND
    }

    fn initial(&self, _input: &Value) -> Value {
        encode(&Checkpoint::prepare(1, None))
    }

    async fn run_phase(&self, task: &RunningTask, rt: &TaskRuntime) -> Result<(), TaskError> {
        match decode(task)? {
            Checkpoint::Prepare {
                attempt,
                compacted,
                overflow,
                threshold,
            } => {
                let resumed = Resumed {
                    compacted,
                    overflow,
                    threshold,
                };
                self.prepare(rt, attempt, resumed).await
            }
            Checkpoint::Request {
                attempt,
                compacted,
                revised_from,
                model,
                cutoff,
                limits,
                cache_key,
                max_output_tokens,
            } => {
                self.request(
                    rt,
                    RequestCheckpoint {
                        attempt,
                        compacted,
                        revised_from,
                        model,
                        cutoff,
                        limits,
                        cache_key,
                        max_output_tokens,
                    },
                )
                .await
            }
            Checkpoint::Retry {
                attempt,
                until,
                compacted,
            } => {
                rt.sleep_until(until).await?;
                let conversation = rt.conversation();
                rt.commit(move |tx, _| {
                    Box::pin(async move {
                        let mut live = live::load(tx, conversation).await?;
                        live.generation = Some(GenerationLive {
                            attempt: attempt + 1,
                            message: None,
                            retry: None,
                        });
                        live::store(tx, conversation, &live).await?;
                        Ok(Some(running(encode(&Checkpoint::prepare(
                            attempt + 1,
                            compacted,
                        )))))
                    })
                })
                .await?;
                Ok(())
            }
            Checkpoint::Tools {
                assistant,
                tools,
                pending,
            } => self.tools_phase(rt, assistant, tools, pending).await,
        }
    }

    /// 转换已提交的部分回复；没开始的调用写 `aborted` 错误结果；输入 `unanswered{aborted}`，
    /// 移除 `run` / `generation` / `tools`，以 `aborted` 收场。在它的工具任务都终结之后才跑。
    async fn abort(&self, task: &RunningTask, rt: &TaskRuntime) -> Result<(), TaskError> {
        let checkpoint = decode(task)?;
        let conversation = rt.conversation();
        let unstarted = match &checkpoint {
            Checkpoint::Tools {
                assistant, pending, ..
            } if !pending.is_empty() => self.pending_calls(rt, *assistant, pending).await?,
            _ => Vec::new(),
        };
        rt.commit(move |tx, task| {
            Box::pin(async move {
                let mut live = live::load(tx, conversation).await?;
                live::convert_partial(tx, conversation, &mut live).await?;
                for (call_id, name) in &unstarted {
                    entries::append_harness_error(
                        tx,
                        conversation,
                        call_id,
                        name,
                        "aborted",
                        &format!("工具 {name} 在开始之前被取消"),
                    )
                    .await?;
                }
                live::end_run(tx, &mut live, task.id(), &unanswered("aborted", None)).await?;
                live::store(tx, conversation, &live).await?;
                Ok(Some(aborted(None)))
            })
        })
        .await?;
        Ok(())
    }
}

/// 给 `HarnessHooks` 用：生成任务被调度器写成 faulted / orphaned 时的清理。
pub(super) async fn settle_scheduler_outcome(
    tx: &mut crate::durable::tx::Tx,
    conversation: ConversationId,
    task: TaskId,
    settlement: &SubmissionSettlement,
) -> Result<(), DurableError> {
    let mut live = live::load(tx, conversation).await?;
    if live.run.as_ref().is_none_or(|run| run.task != task) {
        return Ok(());
    }
    live::convert_partial(tx, conversation, &mut live).await?;
    live::end_run(tx, &mut live, task, settlement).await?;
    live::store(tx, conversation, &live).await
}
