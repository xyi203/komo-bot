//! `impl Ledger for Coordinator`：§8.5 的每一段箭头各一个方法。
//!
//! 顺序在每个方法里都写成三步注释：**外置正文（如有）→ JSONL 追加并 sync_all →
//! state.db 事务**。读到一个方法只有两步，那就是漏了一步。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::events::{
    ConversationBoundary, EventPayload, MessageAssistant, RunAbandoned, RunAccepted, RunCancelled,
    RunCompleted, RunFailed, RunQueued, RunStarted, RunWaiting, ToolPlanned, ToolResult,
    ToolStarted,
};
use komo_kernel::traits::{Ledger, LedgerError, StoreError};
use komo_kernel::types::ids::{AttemptId, EventId, ExecutorId, RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::plan::ExecutionPlan;
use komo_kernel::types::refs::{INLINE_ARGUMENT_LIMIT_BYTES, PublishedOutput, ToolResultStatus};
use komo_kernel::types::status::{AttemptState, RunEnd, RunState, WaitReason};
use komo_kernel::types::turn::{AcceptInput, Accepted, AssistantRound, EventBatch, GrantUse};
use time::OffsetDateTime;

use crate::db::{BoxFuture, Db, store_to_ledger};
use crate::models::PolicyGrantRow;
use crate::repos::{calls, outbox, runs, session};
use crate::session_log::{PendingEvent, SessionPaths};

use super::Coordinator;

#[async_trait]
impl Ledger for Coordinator {
    async fn accept_input(&self, input: AcceptInput) -> Result<Accepted, LedgerError> {
        if input.session != self.session {
            return Err(LedgerError::Conflict(format!(
                "这个 Coordinator 是 {} 的，输入属于 {}",
                self.session, input.session
            )));
        }
        let now = self.clock.now();
        let hash = input.input_hash();

        // Job 的工作目录落到 Session 行上（§10 的 `workdir`）。放在预留 Run **之前**：
        // 装配执行段读的是这一行，而 Run 一进队列就可能被领走。写不进去不该让这一条
        // 输入整个失败——目录不对最坏是回到 `workspaces/`，而丢掉输入是丢掉任务。
        if let Some(workdir) = input.workdir.as_ref().map(|p| p.display().to_string()) {
            let session = self.session.clone();
            if let Err(error) = self
                .db
                .with_write_retry(move |ex| {
                    let (session, workdir) = (session.clone(), workdir.clone());
                    Box::pin(
                        async move { session::set_workdir_in(ex, &session, &workdir, now).await },
                    ) as BoxFuture<'_, Result<(), StoreError>>
                })
                .await
            {
                tracing::warn!(%error, session = %self.session, "工作目录没记到会话上");
            }
        }

        // 会话的标题 = **第一条输入**（`sessions.title`，`komo session list` 与 TUI 列表的那
        // 一列）。只填空的会话，所以后来的消息改不掉它。写不进去不该让这条输入失败——
        // 标题是给人看的，输入是任务。
        if let Some(title) = title_for(&input.text) {
            let session = self.session.clone();
            if let Err(error) = self
                .db
                .with_write_retry(move |ex| {
                    let (session, title) = (session.clone(), title.clone());
                    Box::pin(async move {
                        session::set_title_if_empty_in(ex, &session, &title, now).await
                    }) as BoxFuture<'_, Result<(), StoreError>>
                })
                .await
            {
                tracing::warn!(%error, session = %self.session, "标题没记到会话上");
            }
        }

        // ① state.db 用请求键预留 Run ID，状态 ingesting，**仅存输入哈希与来源**。
        let reserved = {
            let input = input.clone();
            let hash = hash.clone();
            let run = RunId::new_at(now);
            self.db
                .with_write_retry(move |ex| {
                    let (input, hash, run) = (input.clone(), hash.clone(), run.clone());
                    Box::pin(async move {
                        if let Some(existing) =
                            runs::find_by_request_key_in(ex, &input.request_key).await?
                        {
                            // 同一请求键重发时返回原 Run，**内容哈希不同则拒绝**（§8.5）。
                            if existing.input_hash != hash.as_str() {
                                return Ok(Reserved::Conflict);
                            }
                            return Ok(Reserved::Existing(Box::new(existing)));
                        }
                        let row = runs::reserve_in(
                            ex,
                            &runs::NewRun {
                                run,
                                session: input.session.clone(),
                                request_key: input.request_key.clone(),
                                input_hash: hash.as_str().to_string(),
                                source: input.source.clone(),
                                // 子 Run 不继承父的消息历史，只带走这份任务；它在库里
                                // 附属于父（`parent_run_id`）并带着父侧那份结果契约
                                // （§8.4）。**来源仍用父那一份**（`input.source`）：委派
                                // 不是一个新的来源，沿用父 Run 的。
                                delegate: input.delegate.clone(),
                                peer: input.peer.as_ref().map(|p| p.to_string()),
                                model: input.model.clone(),
                                effort: input.model.effort.as_ref().map(|e| e.as_str().to_string()),
                                at: now,
                            },
                        )
                        .await?;
                        Ok(Reserved::Fresh(Box::new(row)))
                    }) as BoxFuture<'_, Result<Reserved, StoreError>>
                })
                .await
                .map_err(store_to_ledger)?
        };

        let row = match reserved {
            Reserved::Conflict => {
                return Err(LedgerError::RequestKeyConflict {
                    key: input.request_key.to_string(),
                });
            }
            Reserved::Existing(row) => {
                // 重发：原 Run 原封不动返回，不再追加一条用户消息。
                let seq = self.log.lock().await.last_seq();
                return Ok(Accepted {
                    run: RunId::from_raw(row.id.clone()),
                    session: self.session.clone(),
                    event: row
                        .input_event
                        .clone()
                        .map(EventId::from_raw)
                        .unwrap_or_else(|| EventId::from_raw(String::new())),
                    seq,
                    deduplicated: true,
                });
            }
            Reserved::Fresh(row) => *row,
        };
        let run = RunId::from_raw(row.id.clone());

        // ② JSONL 追加 run.accepted（包含完整输入和绑定 Run ID），同步文件。
        let (text, text_ref) = self.split_text(&input.text).await?;
        let accepted = self
            .append(
                Some(run.clone()),
                EventPayload::RunAccepted(RunAccepted {
                    request_key: input.request_key.clone(),
                    input_hash: hash,
                    text,
                    text_ref,
                    source: input.source.clone(),
                    peer: input.peer.as_ref().map(|p| p.to_string()),
                    model: Some(input.model.model.clone()),
                    effort: input
                        .model
                        .effort
                        .clone()
                        .map(komo_kernel::types::model::EffortSetting::Explicit),
                    // 契约落在**事件**里（§8.4）：重启之后它是子代理唯一的"结果要长什么样"
                    // 的依据，而父侧复验用的就是同一份。
                    delegate: input.delegate.clone(),
                }),
            )
            .await?;
        let input_event = accepted.event.event_id.clone();
        let input_seq = accepted.seq();

        // `run.queued` 也是一条事件（§8.3 的事件族），它引用承载输入的那一条。
        let queued = self
            .append(
                Some(run.clone()),
                EventPayload::RunQueued(RunQueued {
                    input_ref: input_event.clone(),
                }),
            )
            .await?;

        // ③ state.db 事务写入事件引用、queued 与 applied_seq——**一个事务**。
        let accepted_for_tx = accepted.clone();
        let run_for_tx = run.clone();
        let input_event_for_tx = input_event.clone();
        let session = self.session.clone();
        let bytes = self.log.lock().await.byte_len();
        let now = self.clock.now();
        self.db
            .with_write_retry(move |ex| {
                let (accepted, queued, run, input_event, session) = (
                    accepted_for_tx.clone(),
                    queued.clone(),
                    run_for_tx.clone(),
                    input_event_for_tx.clone(),
                    session.clone(),
                );
                Box::pin(async move {
                    session::index_event_in(ex, &accepted).await?;
                    session::index_event_in(ex, &queued).await?;
                    runs::mark_queued_in(ex, &run, &input_event, input_seq, now).await?;
                    session::set_current_run_in(ex, &session, Some(run.to_string()), now).await?;
                    session::advance_applied_in(ex, &session, queued.seq(), bytes, now).await?;
                    Ok(())
                }) as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
            .map_err(store_to_ledger)?;

        Ok(Accepted {
            run,
            session: self.session.clone(),
            event: input_event,
            seq: input_seq,
            deduplicated: false,
        })
    }

    async fn record_round(
        &self,
        run: &RunId,
        round: AssistantRound,
    ) -> Result<Vec<ToolCallId>, LedgerError> {
        // ① 大消息或大参数先持久保存到 payloads（如有）。
        let (text, text_ref) = match (&round.text, &round.text_ref) {
            (Some(text), None) => self.split_text(text).await?,
            (text, reference) => (text.clone(), reference.clone()),
        };
        let tool_calls = self.externalize_arguments(&round.tool_calls).await?;
        let ids: Vec<ToolCallId> = tool_calls.iter().map(|c| c.call_id.clone()).collect();

        // ② JSONL 写入完整 assistant 事件与全部调用计划的内联内容或引用，同步文件。
        //    **这是一个逻辑事件**：避免只恢复出半轮调用（§8.3）。
        let appended = self
            .append(
                Some(run.clone()),
                EventPayload::MessageAssistant(MessageAssistant {
                    round: round.round,
                    text,
                    text_ref,
                    tool_calls: tool_calls.clone(),
                    provider_blocks: round.provider_blocks.clone(),
                    input_tokens: round.usage.input,
                    output_tokens: round.usage.output,
                }),
            )
            .await?;

        // ③ state.db 事务建立该轮 planned 调用与事件索引。
        let session = self.session.clone();
        let run_id = run.clone();
        let number = round.round;
        self.commit(&appended, move |ex, appended, now| {
            let (session, run_id, tool_calls) =
                (session.clone(), run_id.clone(), tool_calls.clone());
            Box::pin(async move {
                for request in &tool_calls {
                    calls::record_request_in(
                        ex,
                        &session,
                        &run_id,
                        number,
                        &appended.event.event_id,
                        request,
                        now,
                    )
                    .await?;
                }
                runs::bump_rounds_in(ex, &run_id, number, now).await?;
                Ok(())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await?;

        Ok(ids)
    }

    async fn start_run(
        &self,
        run: &RunId,
        executor: &ExecutorId,
        generation: u64,
    ) -> Result<(), LedgerError> {
        // ① JSONL 追加 run.started 并同步。
        //
        // **领取与它是两件事**：`RunQueue::claim` / `claim_run` 只改 `runs` 那一行（条件
        // UPDATE，`rows affected` 是胜负的唯一信号，写不出事件）；这条事件由 handler 在
        // 真正开跑时写，所以账本上的 `run.started` 记的是"谁开始跑了"。
        let appended = self
            .append(
                Some(run.clone()),
                EventPayload::RunStarted(RunStarted {
                    executor: executor.clone(),
                    generation,
                }),
            )
            .await?;

        // ② state.db 事务：代次围栏 + 坐实 running + 事件索引 + applied_seq。
        let run_id = run.clone();
        let executor = executor.clone();
        let stale = Arc::new(std::sync::Mutex::new(None::<u64>));
        let sink = stale.clone();
        self.commit(&appended, move |ex, _appended, now| {
            let (run_id, executor, sink) = (run_id.clone(), executor.clone(), sink.clone());
            Box::pin(async move {
                if let Some(current) =
                    runs::start_in(ex, &run_id, &executor, generation, now).await?
                {
                    *sink.lock().expect("旧代次标记") = Some(current);
                }
                Ok(())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await?;

        // 旧代次不是"写失败"——事件与索引都该留下（它确实发生过），但这个执行者必须
        // 知道自己已经不是当班的那一个（§8.7）。
        if let Some(current) = *stale.lock().expect("旧代次标记") {
            return Err(LedgerError::StaleGeneration {
                held: generation,
                current,
            });
        }
        Ok(())
    }

    async fn plan_call(
        &self,
        call: &ToolCallId,
        plan: &ExecutionPlan,
    ) -> Result<EventId, LedgerError> {
        let row = self.run_of_call(call).await?;
        let run = RunId::from_raw(row.run_id.clone());

        // ① 需要外置的准备计划先持久保存。
        let encoded = serde_json::to_vec(plan)
            .map_err(|e| LedgerError::Persist(format!("计划序列化失败：{e}")))?;
        let (inline, plan_ref) = if encoded.len() <= INLINE_ARGUMENT_LIMIT_BYTES {
            (Some(Box::new(plan.clone())), None)
        } else {
            let reference = self.payloads.put(&encoded).await.map_err(store_to_ledger)?;
            (None, Some(reference))
        };

        // ② JSONL 追加 tool.planned 并同步。
        let appended = self
            .append(
                Some(run),
                EventPayload::ToolPlanned(ToolPlanned {
                    call_id: call.clone(),
                    plan_hash: plan.plan_hash(),
                    plan: inline,
                    plan_ref,
                }),
            )
            .await?;

        // ③ state.db 事务记下计划引用、哈希与恢复方式。
        let call_id = call.clone();
        let plan = plan.clone();
        self.commit(&appended, move |ex, appended, now| {
            let (call_id, plan) = (call_id.clone(), plan.clone());
            Box::pin(async move {
                calls::record_plan_in(ex, &call_id, &plan, &appended.event.event_id, now).await
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await?;

        Ok(appended.event.event_id)
    }

    async fn start_call(
        &self,
        call: &ToolCallId,
        plan: &ExecutionPlan,
        grant: Option<GrantUse>,
    ) -> Result<AttemptId, LedgerError> {
        let row = self.run_of_call(call).await?;
        let run = RunId::from_raw(row.run_id.clone());
        let plan_event = row
            .plan_event
            .clone()
            .map(EventId::from_raw)
            .ok_or_else(|| LedgerError::Conflict("必须先 plan_call 才能 start_call".to_string()))?;

        let attempt = AttemptId::new_at(self.clock.now());

        // ① JSONL 追加 tool.started 并同步。
        let appended = self
            .append(
                Some(run),
                EventPayload::ToolStarted(ToolStarted {
                    call_id: call.clone(),
                    attempt_id: attempt.clone(),
                    plan_ref: plan_event,
                    plan_hash: plan.plan_hash(),
                    grant: grant.as_ref().and_then(|g| g.grant.clone()),
                }),
            )
            .await?;

        // ② state.db 事务提交 started、执行尝试、事件索引和首次授权消费——**同一事务**。
        //    返回之后才允许产生真实副作用（§8.5）。
        let call_id = call.clone();
        let attempt_id = attempt.clone();
        let plan_hash = plan.plan_hash();
        let executor = self.executor.clone();
        let grant = grant.clone();
        self.commit(&appended, move |ex, appended, now| {
            let (call_id, attempt_id, plan_hash, executor, grant) = (
                call_id.clone(),
                attempt_id.clone(),
                plan_hash.clone(),
                executor.clone(),
                grant.clone(),
            );
            Box::pin(async move {
                calls::record_started_in(
                    ex,
                    &call_id,
                    &attempt_id,
                    &plan_hash,
                    &appended.event.event_id,
                    executor.as_ref(),
                    None,
                    now,
                )
                .await?;
                // 「首次授权消费」：一次性授权在这里被用掉。范围授权不因一次使用而消耗
                // （§7.2），所以只动 `once`。
                if let Some(grant) = grant.as_ref().and_then(|g| g.grant.clone())
                    && let Some(mut row) = PolicyGrantRow::filter_by_id(grant.as_str())
                        .first()
                        .exec(ex)
                        .await
                        .map_err(crate::db::map_toasty)?
                    && row.scope_kind == "once"
                    && !row.consumed
                {
                    row.update()
                        .consumed(true)
                        .exec(ex)
                        .await
                        .map_err(crate::db::map_toasty)?;
                }
                Ok(())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await?;

        Ok(attempt)
    }

    async fn finish_call(
        &self,
        attempt: &AttemptId,
        published: PublishedOutput,
    ) -> Result<(), LedgerError> {
        // 输出已由 ToolOutputStore 发布；这里只写元信息与引用（§8.3：JSONL 的
        // tool.result 只保留状态、耗时等元信息、output_ref 和最多 1 KiB 的预览）。
        let attempt_row = {
            let id = attempt.to_string();
            self.db
                .read(move |ex| {
                    let id = id.clone();
                    Box::pin(async move {
                        crate::models::ToolAttemptRow::filter_by_id(&id)
                            .first()
                            .exec(ex)
                            .await
                            .map_err(crate::db::map_toasty)?
                            .ok_or_else(|| StoreError::NotFound {
                                what: format!("attempt {id}"),
                            })
                    })
                        as BoxFuture<'_, Result<crate::models::ToolAttemptRow, StoreError>>
                })
                .await
                .map_err(store_to_ledger)?
        };
        let call = ToolCallId::from_raw(attempt_row.call_id.clone());
        let run = RunId::from_raw(attempt_row.run_id.clone());

        let appended = self
            .append(
                Some(run),
                EventPayload::ToolResult(ToolResult {
                    call_id: call.clone(),
                    attempt_id: attempt.clone(),
                    status: published.status,
                    output_ref: published.output.clone(),
                    elapsed_ms: published.elapsed_ms,
                    preview: published.preview.clone(),
                    stdout: published.stdout.clone(),
                    stderr: published.stderr.clone(),
                    attempt_state: Some(match published.status {
                        ToolResultStatus::Completed => AttemptState::Completed,
                        ToolResultStatus::Failed => AttemptState::Failed,
                        // uncertain：这次**尝试**没有收尾，不能记成失败（§8.6）。
                        ToolResultStatus::Uncertain => AttemptState::Started,
                    }),
                }),
            )
            .await?;

        let attempt_id = attempt.clone();
        self.commit(&appended, move |ex, appended, now| {
            let (attempt_id, published) = (attempt_id.clone(), published.clone());
            Box::pin(async move {
                calls::record_result_in(
                    ex,
                    &attempt_id,
                    published.status,
                    &published.output,
                    published.preview.clone(),
                    &appended.event.event_id,
                    now,
                )
                .await
                .map(|_| ())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
    }

    /// 一次**没有执行过**的调用的结论（工具名不认识、参数准备不出来、放行被拒）。
    ///
    /// 与 [`Self::finish_call`] 的差别只有一处：那条尝试**没有** `tool.started`——所以
    /// attempt 行这里是现造的（`started_event` 留空），而 `tool.result` 该有的东西一样
    /// 不少。少了它，这次调用在账本上永远悬着，转写里那次 `function_call` 永远没有输出
    /// （§8.3 的回放窗口直接 400）。
    async fn fail_call(
        &self,
        call: &ToolCallId,
        attempt: &AttemptId,
        published: PublishedOutput,
    ) -> Result<(), LedgerError> {
        let row = self.run_of_call(call).await?;
        let run = RunId::from_raw(row.run_id.clone());

        let appended = self
            .append(
                Some(run),
                EventPayload::ToolResult(ToolResult {
                    call_id: call.clone(),
                    attempt_id: attempt.clone(),
                    status: published.status,
                    output_ref: published.output.clone(),
                    elapsed_ms: published.elapsed_ms,
                    preview: published.preview.clone(),
                    stdout: published.stdout.clone(),
                    stderr: published.stderr.clone(),
                    // 没有跑过就没有"没跑完"这回事：结论是确定的失败。
                    attempt_state: Some(AttemptState::Failed),
                }),
            )
            .await?;

        let call_id = call.clone();
        let attempt_id = attempt.clone();
        self.commit(&appended, move |ex, appended, now| {
            let (call_id, attempt_id, published) =
                (call_id.clone(), attempt_id.clone(), published.clone());
            Box::pin(async move {
                calls::record_unstarted_in(ex, &call_id, &attempt_id, now).await?;
                calls::record_result_in(
                    ex,
                    &attempt_id,
                    published.status,
                    &published.output,
                    published.preview.clone(),
                    &appended.event.event_id,
                    now,
                )
                .await
                .map(|_| ())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
    }

    async fn suspend(&self, run: &RunId, wait: WaitReason) -> Result<(), LedgerError> {
        // 「停在哪个调用上」**不在这条事件里**：审批的调用在 `approval_requests.call_id`
        // 上，结果不明的调用在 `tool_calls.state = 'uncertain'` 那一条上——两处各自是权威，
        // 事件里再存一份只会和它们漂移（§7.5）。
        let payload = EventPayload::RunWaiting(RunWaiting {
            reason: wait.clone(),
        });

        let appended = self.append(Some(run.clone()), payload).await?;

        let run_id = run.clone();
        self.commit(&appended, move |ex, _appended, now| {
            let (run_id, wait) = (run_id.clone(), wait.clone());
            Box::pin(async move {
                // 让出的那一刻**一并交还领取权**（在 `mark_waiting_in` 里），否则
                // `claimed_by IS NULL` 那条候选永远筛不到它，一个"等一会儿"就变成永久
                // 停摆（§7.4）。
                //
                // `last_error` 不再是"为什么停下"的通道：停下这件事现在由 `WaitReason`
                // 说完（`runs.wait_kind / wait_ref / wake_at`），而 `blocked` 条目的正文
                // 由清单当场派生（§7.5）。写一句自由文本只会与它漂移。
                runs::mark_waiting_in(ex, &run_id, &wait, None, now).await
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
    }

    async fn complete(&self, run: &RunId, end: RunEnd) -> Result<(), LedgerError> {
        let state = end.state();
        let (payload, reason) = match &end {
            RunEnd::Completed {
                final_message,
                rounds,
            } => {
                let (text, text_ref) = match final_message {
                    Some(text) => self.split_text(text).await?,
                    None => (None, None),
                };
                (
                    EventPayload::RunCompleted(RunCompleted {
                        final_message: text,
                        final_message_ref: text_ref,
                        rounds: *rounds,
                    }),
                    None,
                )
            }
            RunEnd::Failed { reason } => (
                EventPayload::RunFailed(RunFailed {
                    reason: reason.clone(),
                }),
                Some(reason.clone()),
            ),
            RunEnd::Cancelled { by } => (
                EventPayload::RunCancelled(RunCancelled {
                    by: by.clone().map(komo_kernel::types::chat::PeerId::new),
                }),
                None,
            ),
            RunEnd::Abandoned { by, reason } => (
                EventPayload::RunAbandoned(RunAbandoned {
                    by: by.clone().map(komo_kernel::types::chat::PeerId::new),
                    reason: reason.clone(),
                }),
                reason.clone(),
            ),
        };

        let appended = self.append(Some(run.clone()), payload).await?;

        // 轮数由调用方交进来（`RunEnd::Completed.rounds`）；失败 / 取消没有这个数，
        // 用行上记着的那个——它是 `record_round` 每轮推进的。
        let declared_rounds = match &end {
            RunEnd::Completed { rounds, .. } => Some(*rounds),
            _ => None,
        };
        let run_id = run.clone();
        let session = self.session.clone();
        self.commit(&appended, move |ex, appended, now| {
            let (run_id, session, reason) = (run_id.clone(), session.clone(), reason.clone());
            Box::pin(async move {
                let rounds = match declared_rounds {
                    Some(rounds) => rounds,
                    None => runs::get_in(ex, &run_id)
                        .await?
                        .map(|row| row.rounds.max(0) as u32)
                        .unwrap_or(0),
                };
                runs::mark_final_in(
                    ex,
                    &run_id,
                    state,
                    &appended.event.event_id,
                    rounds,
                    reason,
                    now,
                )
                .await?;
                if state.is_terminal() {
                    session::set_current_run_in(ex, &session, None, now).await?;
                }
                Ok(())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
    }

    async fn read(
        &self,
        session: &SessionId,
        from: Seq,
        limit: u32,
    ) -> Result<EventBatch, LedgerError> {
        // 别的 Session 走不持有写入器的那条路——一个 Coordinator 只握着自己的日志。
        if session != &self.session {
            let paths = SessionPaths::at(
                self.paths
                    .root()
                    .parent()
                    .unwrap_or(self.paths.root())
                    .join(session.as_str()),
            );
            let (events, more) =
                crate::session_log::read_events(&paths, session, from, limit).await?;
            let next = more.then(|| events.last().map(|e| e.seq)).flatten();
            return Ok(EventBatch {
                session: session.clone(),
                events,
                next,
            });
        }

        let mut log = self.log.lock().await;
        let (events, more) = log.read(from, limit).await?;
        let next = more.then(|| events.last().map(|e| e.seq)).flatten();
        Ok(EventBatch {
            session: session.clone(),
            events,
            next,
        })
    }

    /// 一条 Run 的终态（`Ledger::run_end`，§8.6 的委派核对用它）。
    ///
    /// **权威是终态事件的正文，行上的列只是它的派生物。** 顺着行上的 `final_event` 回到
    /// JSONL 把那条 `run.completed` / `run.failed` / … 读回来——父 Run 续跑时要拿这个
    /// 结果，而 `Completed.final_message` 在 `runs` 表上**没有对应的列**，照着列拼一个
    /// 出来与 `complete` 当时给的不是一回事（大正文还可能在 `payloads/` 里）。父侧复验用
    /// 的就是这个返回值，两次读之间不能有差。
    ///
    /// **非终态答 `None`**（连行都不在也答 `None`：没有这条 Run，就没有它的终态）。剩下
    /// 两条"读不到事件"的路——`final_event` 为空、索引里查不到那条事件——退化成按行上的
    /// 列回答，[`end_from_row`] 写清了哪些字段拿得回来、哪些拿不回来。
    async fn run_end(&self, run: &RunId) -> Result<Option<RunEnd>, LedgerError> {
        let Some(row) = runs::get(&self.db, run).await.map_err(store_to_ledger)? else {
            return Ok(None);
        };
        if !row.state.is_terminal() {
            return Ok(None);
        }
        // 没有终态事件：`mark_final_without_event_in` 那条路（会话已经读不出来时的一次
        // 取消）。取消是调度事实，本来就没有会话内容落在它身上，所以这里不是异常。
        let Some(final_event) = row.final_event.clone() else {
            return Ok(end_from_row(&row));
        };
        // `session_log_index` 只是**加速**索引（§8.3：它可重建），所以查不到不是损坏——
        // 补索引是 reconcile 的事，不该让一次只读被它挡住。
        let Some(seq) = existing_seq(&self.db, &final_event).await? else {
            tracing::warn!(
                %run,
                event = %final_event,
                "会话日志索引里没有这条终态事件，按行上的列回答"
            );
            return Ok(end_from_row(&row));
        };
        // 从 seq 的前一条起读一页，翻出这条事件；它必须**就是** `final_event` 指的那条，
        // 否则读到的是别的东西（索引与日志漂了），一样退化成按列回答。
        let batch = self
            .read(&row.session, Seq(seq.0.saturating_sub(1)), 1)
            .await?;
        let Some(event) = batch
            .events
            .into_iter()
            .find(|event| event.event_id == final_event)
        else {
            tracing::warn!(
                %run,
                event = %final_event,
                seq = seq.0,
                "终态事件在日志里读不回来，按行上的列回答"
            );
            return Ok(end_from_row(&row));
        };

        let end = match event.payload {
            EventPayload::RunCompleted(body) => {
                // 大正文外置过（§8.3）：按引用读回来，缺文件 / 哈希不符会在这里报
                // `Corrupt`——那是真的读不出来，而一个"完成了、但正文丢了"的答案
                // 不能靠少给一段正文冒充。
                let final_message = match (body.final_message, body.final_message_ref) {
                    (Some(text), _) => Some(text),
                    (None, Some(reference)) => Some(self.text_of(&row.session, &reference).await?),
                    (None, None) => None,
                };
                RunEnd::Completed {
                    final_message,
                    rounds: body.rounds,
                }
            }
            EventPayload::RunFailed(body) => RunEnd::Failed {
                reason: body.reason,
            },
            EventPayload::RunCancelled(body) => RunEnd::Cancelled {
                by: body.by.map(|peer| peer.as_str().to_string()),
            },
            EventPayload::RunAbandoned(body) => RunEnd::Abandoned {
                by: body.by.map(|peer| peer.as_str().to_string()),
                reason: body.reason,
            },
            // 索引指到了一条不是终态的事件：与"读不到"同等处理，不猜。
            other => {
                tracing::warn!(
                    %run,
                    event = %final_event,
                    kind = other.type_name(),
                    "终态事件引用的不是终态载荷，按行上的列回答"
                );
                return Ok(end_from_row(&row));
            }
        };
        Ok(Some(end))
    }

    async fn boundary(&self, session: &SessionId) -> Result<Seq, LedgerError> {
        if session != &self.session {
            return Err(LedgerError::Conflict(format!(
                "这个 Coordinator 是 {} 的，边界要划在 {session}",
                self.session
            )));
        }
        // `/new`：追加一个 conversation.boundary，**不切 Session**（§13.1）。
        let appended = self
            .append(
                None,
                EventPayload::ConversationBoundary(ConversationBoundary { by: None }),
            )
            .await?;
        self.commit(&appended, |_ex, _appended, _now| {
            Box::pin(async move { Ok(()) }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await?;
        Ok(appended.seq())
    }

    async fn append_audit(
        &self,
        session: &SessionId,
        event_id: &EventId,
        payload: EventPayload,
        occurred_at: OffsetDateTime,
    ) -> Result<Seq, LedgerError> {
        if session != &self.session {
            return Err(LedgerError::Conflict(format!(
                "这个 Coordinator 是 {} 的，审计事件属于 {session}",
                self.session
            )));
        }

        // 按 event_id 幂等：已写入就**复用原事件位置**（§8.5）。索引是权威，因为它和
        // 追加在同一个提交里推进。
        let digests = session::digests(&self.db, session)
            .await
            .map_err(store_to_ledger)?;
        let _ = digests;
        if let Some(seq) = existing_seq(&self.db, event_id).await? {
            return Ok(seq);
        }

        let appended = {
            let mut log = self.log.lock().await;
            log.append(PendingEvent::new(
                event_id.clone(),
                None,
                // 补写保留**原始发生时间**，不凭日志行相邻推断审批关系（§8.3）。
                occurred_at,
                payload,
            ))
            .await?
        };

        let event_id = event_id.clone();
        self.commit(&appended, move |ex, _appended, _now| {
            let event_id = event_id.clone();
            Box::pin(async move {
                // state.db 标记 outbox 已交付并更新日志索引（§8.5 第三步）。落在哪个 seq
                // 由 `session_log_index` 那一行说——`commit` 刚在同一个事务里写过它。
                outbox::mark_delivered_in(ex, &event_id).await
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await?;

        Ok(appended.seq())
    }
}

enum Reserved {
    Fresh(Box<crate::models::RunRow>),
    Existing(Box<crate::models::RunRow>),
    Conflict,
}

/// 没有终态事件可读时，按行上的列回答（`RunEnd` 的两条退化路径，见
/// [`Ledger::run_end`](komo_kernel::traits::Ledger::run_end) 的实现）。
///
/// **这是一次退化，不是等价替换**：`complete` 把"事件"和"行"一起写，行上有 `state` /
/// `last_error` / `rounds`，所以失败原因、轮数拿得回来；而
/// [`RunEnd::Completed`] 的 `final_message` 在行上**没有列**——它只存在于事件里（大正文
/// 还可能在 `payloads/` 里），这里给 `None`，不从 `message.assistant` 里凑一句：凑出来的
/// 那句与 `complete` 当时给的不是同一个值，而父侧复验照它判，那就等于事后替它编了一个
/// 结果。`by` 同理，行上没记是谁取消的。
fn end_from_row(row: &runs::RunRecord) -> Option<RunEnd> {
    Some(match row.state {
        RunState::Completed => RunEnd::Completed {
            final_message: None,
            rounds: row.rounds,
        },
        RunState::Failed => RunEnd::Failed {
            reason: row.last_error.clone().unwrap_or_default(),
        },
        RunState::Cancelled => RunEnd::Cancelled { by: None },
        RunState::Abandoned => RunEnd::Abandoned {
            by: None,
            reason: row.last_error.clone(),
        },
        // 非终态：调用点已经用 `is_terminal()` 挡过，这里再答一次 `None`，而不是编一个终态
        // 出来——两处判定同源，多出来的这一道只是不让"判定漂了"变成一个假答案。
        _ => return None,
    })
}

async fn existing_seq(db: &Db, event_id: &EventId) -> Result<Option<Seq>, LedgerError> {
    let id = event_id.to_string();
    db.read(move |ex| {
        let id = id.clone();
        Box::pin(async move {
            let row = crate::models::SessionLogIndexRow::filter_by_id(&id)
                .first()
                .exec(ex)
                .await
                .map_err(crate::db::map_toasty)?;
            Ok(row.map(|row| Seq(row.seq.max(0) as u64)))
        }) as BoxFuture<'_, Result<Option<Seq>, StoreError>>
    })
    .await
    .map_err(store_to_ledger)
}

/// 会话标题取哪一段：输入的**第一行**（去掉首尾空白），最多 [`TITLE_MAX_CHARS`] 个字符。
///
/// 空白输入没有标题可言（返回 `None`，于是空会话在列表里仍然是"无标题"，而不是一行空白）。
/// 截断按**字符**切，不按字节——中文按字节切会切出半个字。
fn title_for(text: &str) -> Option<String> {
    let first = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let mut title: String = first.chars().take(TITLE_MAX_CHARS).collect();
    if first.chars().count() > TITLE_MAX_CHARS {
        title.push('…');
    }
    Some(title)
}

/// 标题最多这么多个字符。够列表里认出是哪个会话，又不至于把整段话塞进一列。
const TITLE_MAX_CHARS: usize = 60;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use komo_kernel::events::ApprovalDecided;
    use komo_kernel::test_support::{MemLedger, TestClock, sample_model, sample_plan};
    use komo_kernel::traits::Clock as _;
    use komo_kernel::traits::RunQueue as _;
    use komo_kernel::traits::ToolOutputStore as _;
    use komo_kernel::types::chat::ApprovalScope;
    use komo_kernel::types::delegate::{DelegateSpec, SchemaMode};
    use komo_kernel::types::ids::{ApprovalId, RequestKey};
    use komo_kernel::types::plan::PlanSource;
    use komo_kernel::types::refs::{ContentRef, OutputRef, ToolResultBody};
    use komo_kernel::types::status::{RetryCause, RunState, WaitReason};
    use komo_kernel::types::turn::ToolCallRequest;

    use crate::session_log::TailRepair;

    struct Fixture {
        _dir: tempfile::TempDir,
        db: Db,
        clock: TestClock,
        session: SessionId,
        coordinator: Coordinator,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect(dir.path().join("state.db"))
            .await
            .expect("打开库");
        let clock = TestClock::fixed();
        let session = SessionId::from_raw("00000000-0000-7000-8000-000000000001");
        let coordinator = Coordinator::open(
            db.clone(),
            dir.path().join("sessions"),
            session.clone(),
            "api",
            Arc::new(clock.clone()),
        )
        .await
        .expect("打开 Coordinator");
        Fixture {
            _dir: dir,
            db,
            clock,
            session,
            coordinator,
        }
    }

    fn input(key: &str, text: &str, session: &SessionId) -> AcceptInput {
        AcceptInput {
            session: session.clone(),
            request_key: RequestKey::new(key),
            text: text.into(),
            source: PlanSource::Interactive {
                session: session.clone(),
            },
            peer: None,
            model: sample_model(),
            workdir: None,
            at: time::macros::datetime!(2026-09-15 08:00:00 UTC),
            delegate: None,
        }
    }

    fn call(id: &str) -> ToolCallRequest {
        ToolCallRequest {
            call_id: ToolCallId::from_raw(id),
            provider_call_id: format!("pc-{id}"),
            name: "read".into(),
            arguments: serde_json::json!({"path": "a.txt"}),
            arguments_ref: None,
        }
    }

    fn round(number: u32, calls: Vec<ToolCallRequest>) -> AssistantRound {
        AssistantRound {
            round: number,
            text: Some("好的".into()),
            text_ref: None,
            tool_calls: calls,
            provider_blocks: None,
            usage: Default::default(),
        }
    }

    fn published() -> PublishedOutput {
        PublishedOutput {
            output: OutputRef(ContentRef {
                path: "tool-output/r/c/a/output.json".into(),
                size: 2,
                hash: komo_kernel::types::digest::ContentHash::of_str("{}"),
                pointer: None,
            }),
            status: ToolResultStatus::Completed,
            elapsed_ms: 12,
            preview: Some("ok".into()),
            stdout: None,
            stderr: None,
        }
    }

    async fn types(coordinator: &Coordinator, session: &SessionId) -> Vec<String> {
        let batch = coordinator.read(session, Seq::ZERO, 0).await.unwrap();
        batch
            .events
            .iter()
            .map(|e| e.type_name().to_string())
            .collect()
    }

    /// 会话的标题是**第一条输入**，而且只写一次（`komo session list` 那一列不再全是
    /// 「（无标题）」）。空白输入不产生标题。
    #[tokio::test]
    async fn the_first_input_becomes_the_session_title_once() {
        let f = fixture().await;
        f.coordinator
            .accept_input(input("api:1", "  空调状态\n顺便看看湿度  ", &f.session))
            .await
            .unwrap();
        let row = session::get(&f.db, &f.session).await.unwrap().unwrap();
        assert_eq!(row.title, "空调状态", "取第一行，去掉首尾空白");

        f.coordinator
            .accept_input(input("api:2", "热水器呢", &f.session))
            .await
            .unwrap();
        let row = session::get(&f.db, &f.session).await.unwrap().unwrap();
        assert_eq!(row.title, "空调状态", "后来的消息不改写已有的标题");
    }

    /// 长标题按**字符**截断（中文按字节切会切出半个字）。
    #[test]
    fn a_long_title_is_cut_on_a_character_boundary() {
        let long = "很".repeat(TITLE_MAX_CHARS + 10);
        let title = title_for(&long).unwrap();
        assert_eq!(
            title.chars().count(),
            TITLE_MAX_CHARS + 1,
            "60 个字加一个省略号"
        );
        assert!(title.ends_with('…'));
        assert_eq!(title_for("   \n  \t "), None, "空白输入没有标题");
        assert_eq!(title_for("就好"), Some("就好".into()));
    }

    /// §8.5 的一整条箭头走一遍：输入 → 一轮回复 → 计划 → 开始 → 结果 → 终态。
    #[tokio::test]
    async fn one_turn_lands_in_the_order_the_design_requires() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", "读一下 a.txt", &f.session))
            .await
            .unwrap();
        assert!(!accepted.deduplicated);

        let ids = f
            .coordinator
            .record_round(&accepted.run, round(1, vec![call("call-1")]))
            .await
            .unwrap();
        assert_eq!(ids, vec![ToolCallId::from_raw("call-1")]);

        let plan = sample_plan("read", &f.session);
        let plan_event = f.coordinator.plan_call(&ids[0], &plan).await.unwrap();

        let attempt = f
            .coordinator
            .start_call(&ids[0], &plan, None)
            .await
            .unwrap();
        f.coordinator
            .finish_call(&attempt, published())
            .await
            .unwrap();
        f.coordinator
            .complete(
                &accepted.run,
                RunEnd::Completed {
                    final_message: Some("读好了".into()),
                    rounds: 1,
                },
            )
            .await
            .unwrap();

        assert_eq!(
            types(&f.coordinator, &f.session).await,
            vec![
                "run.accepted",
                "run.queued",
                "message.assistant",
                "tool.planned",
                "tool.started",
                "tool.result",
                "run.completed",
            ]
        );

        // 数据库侧：状态、事件引用与 applied_seq 都跟上了。
        let record = crate::repos::runs::get(&f.db, &accepted.run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.state, RunState::Completed);
        assert_eq!(record.input_event, Some(accepted.event.clone()));
        assert!(record.final_event.is_some());
        assert_eq!(
            record.memory_work,
            komo_kernel::types::memory::MemoryWork::Pending
        );

        let session_row = crate::repos::session::get(&f.db, &f.session)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session_row.applied_seq, Seq(7));
        assert!(session_row.current_run.is_none(), "终态之后当前 Run 清掉");

        // `tool.started` 指向承载计划的那一条事件。
        let batch = f.coordinator.read(&f.session, Seq::ZERO, 0).await.unwrap();
        let started = batch
            .events
            .iter()
            .find(|e| e.type_name() == "tool.started")
            .unwrap();
        let EventPayload::ToolStarted(body) = &started.payload else {
            panic!()
        };
        assert_eq!(body.plan_ref, plan_event);
        assert_eq!(body.attempt_id, attempt);
    }

    /// `run.started` 是 handler 开跑时写的，不是领取时写的。
    #[tokio::test]
    async fn starting_a_run_records_the_executor_and_its_generation() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", "你好", &f.session))
            .await
            .unwrap();

        // 领取只改行——它写不出事件（条件 UPDATE 的 rows affected 是胜负的唯一信号）。
        let queue = crate::repos::queue::TursoRunQueue::new(f.db.clone());
        let executor = ExecutorId::from_raw("exec-1");
        let claimed = queue
            .claim_run(&accepted.run, &executor)
            .await
            .unwrap()
            .expect("领得到");
        assert_eq!(
            types(&f.coordinator, &f.session).await,
            vec!["run.accepted", "run.queued"],
            "领取不写事件"
        );

        f.coordinator
            .start_run(&accepted.run, &executor, claimed.generation)
            .await
            .unwrap();

        assert_eq!(
            types(&f.coordinator, &f.session).await.last().unwrap(),
            "run.started"
        );
        let batch = f.coordinator.read(&f.session, Seq::ZERO, 0).await.unwrap();
        let started = batch
            .events
            .iter()
            .find(|e| e.type_name() == "run.started")
            .unwrap();
        let EventPayload::RunStarted(body) = &started.payload else {
            panic!()
        };
        assert_eq!(body.executor, executor);
        assert_eq!(body.generation, claimed.generation);

        let record = crate::repos::runs::get(&f.db, &accepted.run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.state, RunState::Running);
    }

    /// §8.4 的次序**在受理那一刻**就定下来：前一条还没结束 → 新 Run 落库就是
    /// `waiting + dependency`，不存在"先 queued 再补写"的窗口。
    ///
    /// 少了这条，窗口里调度器能把它领走——那就越过了前面那条未完成的 Run，回放窗口会拿
    /// 一个没有输出的 `function_call` 去问模型（provider 400）。这里从头到尾都断言它领
    /// 不走：受理之后立刻不是候选、`claim_run` 也拿不到；前一条终态之后一次放行才可领。
    #[tokio::test]
    async fn a_run_accepted_behind_an_unfinished_one_lands_waiting_on_it_atomically() {
        let f = fixture().await;
        let queue = crate::repos::queue::TursoRunQueue::new(f.db.clone());
        let executor = ExecutorId::from_raw("exec-1");

        let first = f
            .coordinator
            .accept_input(input("api:1", "先来", &f.session))
            .await
            .unwrap();
        // **不推进时钟**：同一瞬间受理的两条也必须按 seq 排对（次序的权威是输入事件的
        // seq，§8.3），而 Run ID 的字典序在这里是随机的。
        let second = f
            .coordinator
            .accept_input(input("api:2", "后来", &f.session))
            .await
            .unwrap();

        // 落库的形状：受理返回时它**已经是** dependency 等待，没有"先 queued"的那一刻。
        let row = crate::repos::runs::get(&f.db, &second.run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, RunState::Waiting);
        assert_eq!(
            row.wait,
            Some(WaitReason::Dependency {
                run: first.run.clone(),
            })
        );
        assert!(row.input_event.is_some(), "输入的事件引用也落了");

        // 从头到尾领不到它。
        let now = f.clock.now();
        assert!(!queue.due(now, 10).await.unwrap().contains(&second.run));
        assert!(
            queue
                .claim_run(&second.run, &executor)
                .await
                .unwrap()
                .is_none(),
            "前面的没结束，后面的领不走"
        );

        // 前一条终态 → 一次放行 → 可领。
        f.coordinator
            .complete(
                &first.run,
                RunEnd::Completed {
                    final_message: None,
                    rounds: 1,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            crate::repos::queue::release_satisfied_dependencies(&f.db, now)
                .await
                .unwrap(),
            1
        );
        let row = crate::repos::runs::get(&f.db, &second.run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, RunState::Queued);
        assert!(row.wait.is_none());
        assert!(
            queue
                .claim_run(&second.run, &executor)
                .await
                .unwrap()
                .is_some()
        );
    }

    /// 前面那条已经终态了就不挡：新 Run 照旧直接 `queued`。
    #[tokio::test]
    async fn a_run_accepted_after_the_earlier_one_ended_is_queued_right_away() {
        let f = fixture().await;
        let first = f
            .coordinator
            .accept_input(input("api:1", "先来", &f.session))
            .await
            .unwrap();
        f.coordinator
            .complete(
                &first.run,
                RunEnd::Completed {
                    final_message: None,
                    rounds: 1,
                },
            )
            .await
            .unwrap();

        let second = f
            .coordinator
            .accept_input(input("api:2", "后来", &f.session))
            .await
            .unwrap();
        let row = crate::repos::runs::get(&f.db, &second.run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, RunState::Queued);
        assert!(row.wait.is_none());
    }

    /// 旧代次开不了跑——§8.7：停止这个任务的一切写入，不重试、不降级。
    #[tokio::test]
    async fn a_stale_executor_cannot_start_the_run() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", "你好", &f.session))
            .await
            .unwrap();
        let queue = crate::repos::queue::TursoRunQueue::new(f.db.clone());
        let first = ExecutorId::from_raw("exec-1");
        let claimed = queue
            .claim_run(&accepted.run, &first)
            .await
            .unwrap()
            .unwrap();
        // 交还名额只是交还名额（§8.7）：要让它重新可领取得走恢复那条路——reconcile 判成
        // `queued`。这里直接用 `requeue`（它就是那个结论）。
        queue.release(&claimed).await.unwrap();
        crate::repos::recovery::RecoveryStore::new(f.db.clone(), std::path::PathBuf::from("."))
            .requeue(&accepted.run)
            .await
            .unwrap();
        let second = ExecutorId::from_raw("exec-2");
        queue
            .claim_run(&accepted.run, &second)
            .await
            .unwrap()
            .unwrap();

        let error = f
            .coordinator
            .start_run(&accepted.run, &first, claimed.generation)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            LedgerError::StaleGeneration {
                held: 1,
                current: 2
            }
        );
    }

    /// 停在一条审批上：事件里带的是**等什么**（`WaitReason`），行上是 `waiting` + 三列。
    #[tokio::test]
    async fn waiting_for_approval_writes_the_reason_both_to_the_event_and_the_row() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", "跑一下", &f.session))
            .await
            .unwrap();
        f.coordinator
            .suspend(
                &accepted.run,
                WaitReason::Approval {
                    approval: ApprovalId::from_raw("ap-1"),
                },
            )
            .await
            .unwrap();

        let batch = f.coordinator.read(&f.session, Seq::ZERO, 0).await.unwrap();
        let EventPayload::RunWaiting(body) = &batch.events.last().unwrap().payload else {
            panic!()
        };
        assert_eq!(body.reason.kind(), "approval");
        assert_eq!(body.reason.reference().as_deref(), Some("ap-1"));

        let record = crate::repos::runs::get(&f.db, &accepted.run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.state, RunState::Waiting);
        assert_eq!(
            record.wait,
            Some(WaitReason::Approval {
                approval: ApprovalId::from_raw("ap-1"),
            }),
        );
    }

    /// 轮数由调用方交进来，事件与行上是同一个数——`RunCompleted.rounds` 不再恒为 0。
    #[tokio::test]
    async fn the_round_count_reaches_both_the_event_and_the_row() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", "算一下", &f.session))
            .await
            .unwrap();
        f.coordinator
            .complete(
                &accepted.run,
                RunEnd::Completed {
                    final_message: Some("等于 2".into()),
                    rounds: 3,
                },
            )
            .await
            .unwrap();

        let batch = f.coordinator.read(&f.session, Seq::ZERO, 0).await.unwrap();
        let EventPayload::RunCompleted(body) = &batch.events.last().unwrap().payload else {
            panic!()
        };
        assert_eq!(body.rounds, 3);
        assert_eq!(
            crate::repos::runs::get(&f.db, &accepted.run)
                .await
                .unwrap()
                .unwrap()
                .rounds,
            3
        );
    }

    /// 同一请求键重发返回原 Run；内容哈希不同则拒绝（§8.5）。
    #[tokio::test]
    async fn the_same_request_key_returns_the_same_run_and_refuses_different_content() {
        let f = fixture().await;
        let first = f
            .coordinator
            .accept_input(input("telegram:7", "你好", &f.session))
            .await
            .unwrap();
        let again = f
            .coordinator
            .accept_input(input("telegram:7", "你好", &f.session))
            .await
            .unwrap();
        assert_eq!(again.run, first.run);
        assert!(again.deduplicated);
        assert_eq!(
            types(&f.coordinator, &f.session).await,
            vec!["run.accepted", "run.queued"],
            "重发不再追加一条用户消息"
        );

        let error = f
            .coordinator
            .accept_input(input("telegram:7", "你好吗", &f.session))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            LedgerError::RequestKeyConflict {
                key: "telegram:7".into()
            }
        );
    }

    /// 超限正文外置到 `payloads/`，JSONL 里只留引用（§8.3）。
    #[tokio::test]
    async fn an_oversized_body_is_externalised_and_the_reference_verifies() {
        let f = fixture().await;
        let big = "汉".repeat(INLINE_ARGUMENT_LIMIT_BYTES);
        let accepted = f
            .coordinator
            .accept_input(input("api:big", &big, &f.session))
            .await
            .unwrap();
        let _ = accepted;

        let batch = f.coordinator.read(&f.session, Seq::ZERO, 0).await.unwrap();
        let EventPayload::RunAccepted(body) = &batch.events[0].payload else {
            panic!()
        };
        assert!(body.text.is_none(), "大正文不内联");
        let reference = body.text_ref.clone().expect("有引用");
        let bytes = f.coordinator.payloads().open(&reference).await.unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), big);
    }

    /// 超限的调用参数外置，`arguments_ref` 指向文件内的对应字段（§8.3）。
    #[tokio::test]
    async fn oversized_arguments_are_externalised_with_a_pointer() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:args", "跑一下", &f.session))
            .await
            .unwrap();
        let mut request = call("call-big");
        request.arguments = serde_json::json!({ "code": "x".repeat(INLINE_ARGUMENT_LIMIT_BYTES) });
        f.coordinator
            .record_round(&accepted.run, round(1, vec![request.clone()]))
            .await
            .unwrap();

        let batch = f.coordinator.read(&f.session, Seq::ZERO, 0).await.unwrap();
        let EventPayload::MessageAssistant(body) = &batch.events[2].payload else {
            panic!()
        };
        let stored = &body.tool_calls[0];
        assert!(stored.arguments.is_null());
        let reference = stored.arguments_ref.clone().expect("有引用");
        assert_eq!(
            reference.0.pointer.as_deref(),
            Some("/tool_calls/0/arguments")
        );
        let bytes = f.coordinator.payloads().open(&reference).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            request.arguments
        );
    }

    /// 验收 ⑧：`read` 的分页语义与 `MemLedger` 一致——`next=None` 就是读到头了。
    #[tokio::test]
    async fn paging_matches_the_in_memory_ledger() {
        let f = fixture().await;
        let clock = TestClock::fixed();
        let mem = MemLedger::new(clock);

        for index in 0..3 {
            let payload = input(
                &format!("api:{index}"),
                &format!("第 {index} 条"),
                &f.session,
            );
            f.coordinator.accept_input(payload.clone()).await.unwrap();
            mem.accept_input(payload).await.unwrap();
        }

        for limit in [1u32, 2, 3, 6, 100] {
            let mut cursor = Seq::ZERO;
            let mut mine = Vec::new();
            let mut theirs = Vec::new();
            loop {
                let a = f.coordinator.read(&f.session, cursor, limit).await.unwrap();
                let b = mem.read(&f.session, cursor, limit).await.unwrap();
                assert_eq!(
                    a.events.len(),
                    b.events.len(),
                    "limit={limit} cursor={cursor} 的一页大小要一致"
                );
                assert_eq!(a.next, b.next, "limit={limit} cursor={cursor} 的游标要一致");
                mine.extend(a.events.iter().map(|e| e.seq));
                theirs.extend(b.events.iter().map(|e| e.seq));
                match a.next {
                    Some(next) => cursor = next,
                    None => break,
                }
            }
            assert_eq!(mine, theirs, "limit={limit} 读出来的 seq 序列要一致");
            assert_eq!(mine.len(), 6);
        }

        // 读到头：`next` 是 `None`，不是一个"总是有值的游标"。
        let tail = f.coordinator.read(&f.session, Seq(6), 10).await.unwrap();
        assert!(tail.events.is_empty());
        assert!(!tail.has_more());
        assert_eq!(tail.next, None);
    }

    /// 验收 ⑩：`append_audit` 按 `event_id` 幂等——已写入就**复用原事件位置**（§8.5）。
    #[tokio::test]
    async fn appending_the_same_audit_event_twice_reuses_its_position() {
        let f = fixture().await;
        f.coordinator
            .accept_input(input("api:1", "你好", &f.session))
            .await
            .unwrap();

        let event_id = EventId::from_raw("00000000-0000-7000-8000-0000000000aa");
        let decided_at = time::macros::datetime!(2026-09-14 20:30:00 UTC);
        let payload = EventPayload::ApprovalDecided(ApprovalDecided {
            approval: ApprovalId::from_raw("ap-1"),
            approved: true,
            scope: ApprovalScope::Once,
            by: Some(komo_kernel::types::chat::PeerId::new("operator")),
            decided_at,
            grant: None,
        });

        let first = f
            .coordinator
            .append_audit(&f.session, &event_id, payload.clone(), decided_at)
            .await
            .unwrap();
        // 中间又发生了别的事，位置不能因此漂移。
        f.coordinator.boundary(&f.session).await.unwrap();
        let second = f
            .coordinator
            .append_audit(&f.session, &event_id, payload, decided_at)
            .await
            .unwrap();
        assert_eq!(first, second, "复用原事件位置，不追加第二条");

        let batch = f.coordinator.read(&f.session, Seq::ZERO, 0).await.unwrap();
        assert_eq!(
            batch
                .events
                .iter()
                .filter(|e| e.type_name() == "approval.decided")
                .count(),
            1
        );
        let audit = batch
            .events
            .iter()
            .find(|e| e.event_id == event_id)
            .unwrap();
        assert_eq!(
            audit.ts, decided_at,
            "补写保留**原始发生时间**，不是补写的时间"
        );
    }

    /// `/new` 只是追加一条边界，**不切 Session**（§13.1）。
    #[tokio::test]
    async fn a_boundary_does_not_start_a_new_session() {
        let f = fixture().await;
        f.coordinator
            .accept_input(input("api:1", "你好", &f.session))
            .await
            .unwrap();
        let seq = f.coordinator.boundary(&f.session).await.unwrap();
        assert_eq!(seq, Seq(3));
        assert_eq!(
            types(&f.coordinator, &f.session).await.last().unwrap(),
            "conversation.boundary"
        );
        assert_eq!(
            crate::repos::session::get(&f.db, &f.session)
                .await
                .unwrap()
                .unwrap()
                .applied_seq,
            Seq(3)
        );
    }

    /// 让出执行名额等退避：状态、等待三列与事件都落到位，而且**读回来是同一份等待**。
    #[tokio::test]
    async fn suspending_records_both_the_event_and_the_wait() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", "你好", &f.session))
            .await
            .unwrap();
        let later = f.clock.now() + time::Duration::minutes(5);
        f.coordinator
            .suspend(
                &accepted.run,
                WaitReason::Retry {
                    attempts: 2,
                    not_before: later,
                    cause: RetryCause::Transport,
                },
            )
            .await
            .unwrap();

        let record = crate::repos::runs::get(&f.db, &accepted.run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.state, RunState::Waiting);
        assert_eq!(record.retry_attempts, 2);
        assert_eq!(record.wake_at, Some(later));
        assert_eq!(
            record.wait,
            Some(WaitReason::Retry {
                attempts: 2,
                not_before: later,
                cause: RetryCause::Transport,
            }),
        );
        assert_eq!(
            types(&f.coordinator, &f.session).await.last().unwrap(),
            "run.waiting"
        );
    }

    /// 结果不明的调用留在 `uncertain`，它的尝试仍是 `started`——那次尝试确实没有收尾
    /// （§8.6）。
    #[tokio::test]
    async fn an_uncertain_result_does_not_close_the_call() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", "跑一下", &f.session))
            .await
            .unwrap();
        let ids = f
            .coordinator
            .record_round(&accepted.run, round(1, vec![call("call-1")]))
            .await
            .unwrap();
        let plan = sample_plan("shell", &f.session);
        f.coordinator.plan_call(&ids[0], &plan).await.unwrap();
        let attempt = f
            .coordinator
            .start_call(&ids[0], &plan, None)
            .await
            .unwrap();

        let mut result = published();
        result.status = ToolResultStatus::Uncertain;
        f.coordinator.finish_call(&attempt, result).await.unwrap();

        let calls = crate::repos::calls::list_for_run(&f.db, &accepted.run)
            .await
            .unwrap();
        assert_eq!(calls[0].state, "uncertain");
        let attempts = crate::repos::calls::attempts_of(&f.db, &ids[0])
            .await
            .unwrap();
        assert_eq!(attempts[0].state, "started", "这次尝试没有收尾");
    }

    /// 没有计划就开不了工——`tool.started` 之前必须有 `tool.planned`（§8.4 第 6 行）。
    #[tokio::test]
    async fn a_call_cannot_start_before_its_plan_is_on_disk() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", "跑一下", &f.session))
            .await
            .unwrap();
        let ids = f
            .coordinator
            .record_round(&accepted.run, round(1, vec![call("call-1")]))
            .await
            .unwrap();
        let plan = sample_plan("shell", &f.session);
        let error = f
            .coordinator
            .start_call(&ids[0], &plan, None)
            .await
            .unwrap_err();
        assert!(matches!(error, LedgerError::Conflict(_)), "{error}");
    }

    /// 重开一个 Coordinator，历史读得回来，seq 接得上。
    #[tokio::test]
    async fn reopening_a_session_continues_where_it_left_off() {
        let f = fixture().await;
        f.coordinator
            .accept_input(input("api:1", "你好", &f.session))
            .await
            .unwrap();
        let root = f._dir.path().join("sessions");
        let db = f.db.clone();
        let session = f.session.clone();
        drop(f.coordinator);

        let again = Coordinator::open(
            db,
            root,
            session.clone(),
            "api",
            Arc::new(TestClock::fixed()),
        )
        .await
        .unwrap();
        assert_eq!(again.last_seq().await, Seq(2));
        assert_eq!(again.tail_repair().await, TailRepair::Clean);

        let batch = again.read(&session, Seq::ZERO, 0).await.unwrap();
        assert_eq!(batch.events.len(), 2);
        let seq = again.boundary(&session).await.unwrap();
        assert_eq!(seq, Seq(3), "seq 从 3 接着走");
    }

    /// 发布过的输出读得回来——`Coordinator::outputs()` 和它是同一个 Session。
    #[tokio::test]
    async fn the_coordinators_output_store_serves_the_same_session() {
        let f = fixture().await;
        let store = f.coordinator.outputs();
        let attempt = komo_kernel::types::refs::AttemptRef {
            session: f.session.clone(),
            run: RunId::from_raw("run-1"),
            call: ToolCallId::from_raw("call-1"),
            attempt: AttemptId::from_raw("attempt-1"),
        };
        let writer = store.begin(&attempt).await.unwrap();
        let out = store
            .publish(
                writer,
                ToolResultBody {
                    status: ToolResultStatus::Completed,
                    result: serde_json::json!("ok"),
                    error: None,
                    exit_code: Some(0),
                    artifacts: vec![],
                },
            )
            .await
            .unwrap();
        assert!(store.open(&out.output).await.is_ok());
    }

    // ------------------------------------------------------------ 委派（§8.4 的 dependency）

    /// 一份子任务输入：`delegate` 指着父 Run 与父侧那次调用。
    fn delegated(
        key: &str,
        parent: &RunId,
        text: &str,
        session: &SessionId,
        contract: Option<serde_json::Value>,
    ) -> AcceptInput {
        let spec = match contract {
            Some(schema) => {
                DelegateSpec::new(parent.clone(), ToolCallId::from_raw("call-delegate"), text)
                    .with_contract(schema, SchemaMode::Strict)
            }
            None => DelegateSpec::new(parent.clone(), ToolCallId::from_raw("call-delegate"), text),
        };
        AcceptInput {
            delegate: Some(spec),
            ..input(key, text, session)
        }
    }

    /// §8.4 的委派：子 Run **落 `queued`**（不是 `waiting + dependency`——父在跑、子若等父
    /// 就是父子互等死锁），父停下来等它的时候它才领得走，它进终态之后父回 `queued`。
    ///
    /// 这一条把委派的整个调度闭环走了一遍，因为四个落点分属三处（`mark_queued_in` 的例外、
    /// §8.7 领取语句的豁免、`release_satisfied_dependencies` 的放行），任何一处漏了都会
    /// 停在这里。
    #[tokio::test]
    async fn a_delegated_child_is_claimable_exactly_while_its_parent_waits_on_it() {
        let f = fixture().await;
        let queue = crate::repos::queue::TursoRunQueue::new(f.db.clone());
        let executor = ExecutorId::from_raw("exec-1");
        let now = f.clock.now();

        let parent = f
            .coordinator
            .accept_input(input("api:1", "派个子任务", &f.session))
            .await
            .unwrap();
        // 契约用 `DelegateSpec` 默认的那份 schema 子集：`required` 是校验器真认的关键字。
        let schema = serde_json::json!({"type": "object", "required": ["lines"]});
        let child = f
            .coordinator
            .accept_input(delegated(
                "api:2",
                &parent.run,
                "数一下 a.txt 有几行",
                &f.session,
                Some(schema.clone()),
            ))
            .await
            .unwrap();

        // ① 落库形状：`queued`，附属于父，契约原样存回来（父侧复验要用的**同一份**）。
        let row = crate::repos::runs::get(&f.db, &child.run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, RunState::Queued, "子 Run 不排父后面等它");
        assert!(row.wait.is_none(), "`queued` 没有等待三列");
        assert_eq!(row.parent, Some(parent.run.clone()));
        assert_eq!(
            row.delegate,
            Some(
                DelegateSpec::new(
                    parent.run.clone(),
                    ToolCallId::from_raw("call-delegate"),
                    "数一下 a.txt 有几行",
                )
                .with_contract(schema, SchemaMode::Strict)
            ),
            "契约整份存回来（含 mode）"
        );

        // 受理事件里也带着它——重启之后那是**唯一**的"结果要长什么样"的依据。
        let batch = f.coordinator.read(&f.session, Seq::ZERO, 0).await.unwrap();
        let accepted = batch
            .events
            .iter()
            .filter(|event| event.run.as_ref() == Some(&child.run))
            .find_map(|event| match &event.payload {
                EventPayload::RunAccepted(body) => Some(body),
                _ => None,
            })
            .expect("子 Run 有一条 run.accepted");
        assert_eq!(accepted.delegate, row.delegate);

        // ② 父还没停下来等它：**领不走**（`queued` 只说明"不缺 worker 以外的条件"）。
        assert!(!queue.due(now, 10).await.unwrap().contains(&child.run));
        assert!(
            queue
                .claim_run(&child.run, &executor)
                .await
                .unwrap()
                .is_none(),
            "父还在跑的这段时间里它领不走"
        );

        // ③ 父开跑、然后停下来等这次委派的结果（§8.4 的 `dependency`）。
        queue
            .claim_run(&parent.run, &executor)
            .await
            .unwrap()
            .expect("父自己没有前置，领得到");
        f.coordinator
            .suspend(
                &parent.run,
                WaitReason::Dependency {
                    run: child.run.clone(),
                },
            )
            .await
            .unwrap();
        assert!(
            queue.due(now, 10).await.unwrap().contains(&child.run),
            "父正等着它 → 它是候选"
        );
        assert!(
            queue
                .claim_run(&child.run, &executor)
                .await
                .unwrap()
                .is_some(),
            "父正等着它 → 领得走（豁免的五个条件在这里全成立）"
        );

        // ④ 子 Run 终态 → 父的依赖等到了 → 父回 `queued`、又能领。
        f.coordinator
            .complete(
                &child.run,
                RunEnd::Completed {
                    final_message: Some("3 行".into()),
                    rounds: 1,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            crate::repos::queue::release_satisfied_dependencies(&f.db, now)
                .await
                .unwrap(),
            1,
            "父是在等子，方向与「前一条 Run」相反，判据同一份"
        );
        let row = crate::repos::runs::get(&f.db, &parent.run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, RunState::Queued);
        assert!(row.wait.is_none());
        assert!(
            queue
                .claim_run(&parent.run, &executor)
                .await
                .unwrap()
                .is_some(),
            "放行之后父领得走"
        );
    }

    /// 委派的子 Run 不继承父的消息历史：它的输入正文就是那份任务本身。
    #[tokio::test]
    async fn a_delegated_child_starts_from_the_task_not_the_parent_history() {
        let f = fixture().await;
        let parent = f
            .coordinator
            .accept_input(input("api:1", "先聊两句天气", &f.session))
            .await
            .unwrap();
        let child = f
            .coordinator
            .accept_input(delegated("api:2", &parent.run, "数行数", &f.session, None))
            .await
            .unwrap();

        let batch = f.coordinator.read(&f.session, Seq::ZERO, 0).await.unwrap();
        let child_types: Vec<String> = batch
            .events
            .iter()
            .filter(|event| event.run.as_ref() == Some(&child.run))
            .map(|event| event.type_name().to_string())
            .collect();
        assert_eq!(
            child_types,
            vec!["run.accepted", "run.queued"],
            "子 Run 只有自己的受理与入队，没有父那一轮里的任何一条"
        );
        let text = batch
            .events
            .iter()
            .filter(|event| event.run.as_ref() == Some(&child.run))
            .find_map(|event| match &event.payload {
                EventPayload::RunAccepted(body) => Some(body.text.clone()),
                _ => None,
            })
            .expect("子 Run 有一条 run.accepted");
        assert_eq!(text.as_deref(), Some("数行数"), "输入正文就是那份任务");
    }

    /// `runs.delegate` 写坏了（或者根本没有这一列的老行）**不算损坏**：当作"没有契约"，
    /// 而"谁派的"是 `parent_run_id` 那一列上的判据，不受影响。
    #[tokio::test]
    async fn a_corrupt_delegate_column_reads_as_no_contract_and_keeps_the_parent() {
        let f = fixture().await;
        let parent = f
            .coordinator
            .accept_input(input("api:1", "派个子任务", &f.session))
            .await
            .unwrap();
        let child = f
            .coordinator
            .accept_input(delegated("api:2", &parent.run, "数行数", &f.session, None))
            .await
            .unwrap();

        let run = child.run.to_string();
        f.db.with_write_retry(move |ex| {
            let run = run.clone();
            Box::pin(async move {
                toasty::sql::statement("UPDATE runs SET delegate = '{ 这不是 JSON' WHERE id = ?1")
                    .bind(run)
                    .exec(ex)
                    .await
                    .map(|_| ())
                    .map_err(crate::db::map_toasty)
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();

        let row = crate::repos::runs::get(&f.db, &child.run)
            .await
            .unwrap()
            .expect("损坏的契约不该让整行读不出来");
        assert_eq!(row.delegate, None, "读不出来就是没有契约，不猜一个出来");
        assert_eq!(
            row.parent,
            Some(parent.run.clone()),
            "判据在 parent_run_id 那一列上"
        );
        assert_eq!(row.state, RunState::Queued, "调度状态一点也不受影响");

        // 旧行：这一列还是 NULL。两列都缺的行与"值为 NULL"的行是同一件事（§8.2 的补列）。
        let legacy = parent.run.to_string();
        f.db.with_write_retry(move |ex| {
            let legacy = legacy.clone();
            Box::pin(async move {
                toasty::sql::statement(
                    "UPDATE runs SET delegate = NULL, parent_run_id = NULL WHERE id = ?1",
                )
                .bind(legacy)
                .exec(ex)
                .await
                .map(|_| ())
                .map_err(crate::db::map_toasty)
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();
        let row = crate::repos::runs::get(&f.db, &parent.run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((row.parent, row.delegate), (None, None), "顶层 Run 的老行");
        assert_eq!(
            crate::repos::runs::list_for_session(&f.db, &f.session)
                .await
                .unwrap()
                .len(),
            2,
            "整份列表照样读得出来"
        );
    }

    /// `run_end` 答的是**账本里那条终态事件**，不是照着行上的列重拼一份。
    #[tokio::test]
    async fn run_end_reads_the_terminal_event_back() {
        let f = fixture().await;

        let completed = f
            .coordinator
            .accept_input(input("api:1", "读一下 a.txt", &f.session))
            .await
            .unwrap();
        f.coordinator
            .complete(
                &completed.run,
                RunEnd::Completed {
                    final_message: Some("三行".into()),
                    rounds: 2,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            f.coordinator.run_end(&completed.run).await.unwrap(),
            Some(RunEnd::Completed {
                final_message: Some("三行".into()),
                rounds: 2,
            })
        );

        let failed = f
            .coordinator
            .accept_input(input("api:2", "再读一次", &f.session))
            .await
            .unwrap();
        f.coordinator
            .complete(
                &failed.run,
                RunEnd::Failed {
                    reason: "read 超时".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            f.coordinator.run_end(&failed.run).await.unwrap(),
            Some(RunEnd::Failed {
                reason: "read 超时".into(),
            })
        );

        // 大正文外置过（§8.3）：按引用读回来，而不是答一个"没有正文"。
        let big = "汉".repeat(INLINE_ARGUMENT_LIMIT_BYTES);
        let externalised = f
            .coordinator
            .accept_input(input("api:3", "写一段长的", &f.session))
            .await
            .unwrap();
        f.coordinator
            .complete(
                &externalised.run,
                RunEnd::Completed {
                    final_message: Some(big.clone()),
                    rounds: 1,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            f.coordinator.run_end(&externalised.run).await.unwrap(),
            Some(RunEnd::Completed {
                final_message: Some(big),
                rounds: 1,
            })
        );

        // 非终态：没有终态事件，答 `None`（不是"编一个"）。
        let open = f
            .coordinator
            .accept_input(input("api:4", "还没跑完", &f.session))
            .await
            .unwrap();
        assert_eq!(f.coordinator.run_end(&open.run).await.unwrap(), None);
        assert_eq!(
            f.coordinator
                .run_end(&RunId::from_raw("run-does-not-exist"))
                .await
                .unwrap(),
            None,
            "行不在也一样：没有这条 Run，就没有它的终态"
        );
    }

    /// 终态**没有事件**（会话已经读不出来时的一次取消）：按行上的列回答，拿不回来的字段
    /// 明着给 `None`，不替 `complete` 编一个。
    #[tokio::test]
    async fn run_end_falls_back_to_the_row_when_the_terminal_event_is_absent() {
        let f = fixture().await;
        let accepted = f
            .coordinator
            .accept_input(input("api:1", "你好", &f.session))
            .await
            .unwrap();
        crate::repos::runs::stop_without_event(
            &f.db,
            &f.session,
            &accepted.run,
            RunState::Cancelled,
            None,
            f.clock.now(),
        )
        .await
        .unwrap();

        let row = crate::repos::runs::get(&f.db, &accepted.run)
            .await
            .unwrap()
            .unwrap();
        assert!(row.final_event.is_none(), "这条终态本来就没有事件");
        assert_eq!(
            f.coordinator.run_end(&accepted.run).await.unwrap(),
            Some(RunEnd::Cancelled { by: None }),
            "取消这个事实在行上有；`by` 没有列可依，所以是 None"
        );

        // 失败也一样：原因是 `last_error` 那一列记着的。
        let failed = f
            .coordinator
            .accept_input(input("api:2", "又一条", &f.session))
            .await
            .unwrap();
        crate::repos::runs::stop_without_event(
            &f.db,
            &f.session,
            &failed.run,
            RunState::Failed,
            Some("日志读不出来".into()),
            f.clock.now(),
        )
        .await
        .unwrap();
        assert_eq!(
            f.coordinator.run_end(&failed.run).await.unwrap(),
            Some(RunEnd::Failed {
                reason: "日志读不出来".into(),
            })
        );
    }
}
