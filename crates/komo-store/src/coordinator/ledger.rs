//! `impl Ledger for Coordinator`：§8.5 的每一段箭头各一个方法。
//!
//! 顺序在每个方法里都写成三步注释：**外置正文（如有）→ JSONL 追加并 sync_all →
//! state.db 事务**。读到一个方法只有两步，那就是漏了一步。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::events::{
    ConversationBoundary, EventPayload, MessageAssistant, RunAccepted, RunCancelled, RunCompleted,
    RunFailed, RunNeedsAttention, RunQueued, RunStarted, RunWaitingApproval, RunWaitingRetry,
    ToolPlanned, ToolResult, ToolStarted,
};
use komo_kernel::traits::{Ledger, LedgerError, StoreError};
use komo_kernel::types::ids::{AttemptId, EventId, ExecutorId, RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::plan::ExecutionPlan;
use komo_kernel::types::refs::{INLINE_ARGUMENT_LIMIT_BYTES, PublishedOutput, ToolResultStatus};
use komo_kernel::types::status::{AttemptState, RunEnd, Wait};
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
                    runs::mark_queued_in(ex, &run, &input_event, now).await?;
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

    async fn suspend(&self, run: &RunId, wait: Wait) -> Result<(), LedgerError> {
        let status = wait.status();
        let payload = match &wait {
            // `attempt` **故意不落**：`approval_requests` 上没有尝试这一列，而
            // `Wait::Approval::attempt` 通常本来就是 None——审批发生在 `tool.started`
            // 之前，那时候一次尝试都还没有。真要记它得先给那张耐久表加一列，不在这一波。
            Wait::Approval {
                approval,
                call,
                attempt: _,
            } => EventPayload::RunWaitingApproval(RunWaitingApproval {
                approval: approval.clone(),
                call: call.clone(),
            }),
            Wait::Retry {
                attempts,
                next_retry_at,
                reason,
            } => EventPayload::RunWaitingRetry(RunWaitingRetry {
                attempts: *attempts,
                next_retry_at: *next_retry_at,
                reason: reason.clone(),
            }),
            Wait::Attention { reason } => EventPayload::RunNeedsAttention(RunNeedsAttention {
                reason: reason.clone(),
                call: None,
            }),
        };

        let appended = self.append(Some(run.clone()), payload).await?;

        let run_id = run.clone();
        let (attempts, next_retry_at, reason) = match &wait {
            Wait::Retry {
                attempts,
                next_retry_at,
                reason,
            } => (*attempts, Some(*next_retry_at), Some(reason.clone())),
            Wait::Attention { reason } => (0, None, Some(reason.clone())),
            Wait::Approval { .. } => (0, None, None),
        };
        self.commit(&appended, move |ex, _appended, now| {
            let (run_id, reason) = (run_id.clone(), reason.clone());
            Box::pin(async move {
                runs::mark_waiting_in(ex, &run_id, status, attempts, next_retry_at, reason, now)
                    .await
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
    }

    async fn complete(&self, run: &RunId, end: RunEnd) -> Result<(), LedgerError> {
        let status = end.status();
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
                    status,
                    &appended.event.event_id,
                    rounds,
                    reason,
                    now,
                )
                .await?;
                if status.is_terminal() {
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
    use komo_kernel::types::ids::{ApprovalId, RequestKey};
    use komo_kernel::types::plan::PlanSource;
    use komo_kernel::types::refs::{ContentRef, OutputRef, ToolResultBody};
    use komo_kernel::types::status::RunStatus;
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
            at: time::macros::datetime!(2026-09-15 08:00:00 UTC),
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
        assert_eq!(record.status, RunStatus::Completed);
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
        assert_eq!(record.status, RunStatus::Running);
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
        queue.release(&claimed).await.unwrap();
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

    /// 停在哪个**逻辑调用**上要透传到事件里——恢复按它配对。
    #[tokio::test]
    async fn waiting_for_approval_names_the_call_it_stopped_on() {
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

        f.coordinator
            .suspend(
                &accepted.run,
                Wait::Approval {
                    approval: ApprovalId::from_raw("ap-1"),
                    call: Some(ids[0].clone()),
                    attempt: None,
                },
            )
            .await
            .unwrap();

        let batch = f.coordinator.read(&f.session, Seq::ZERO, 0).await.unwrap();
        let EventPayload::RunWaitingApproval(body) = &batch.events.last().unwrap().payload else {
            panic!()
        };
        assert_eq!(body.call.as_ref(), Some(&ids[0]));
        assert_eq!(body.approval.as_str(), "ap-1");
        assert_eq!(
            crate::repos::runs::get(&f.db, &accepted.run)
                .await
                .unwrap()
                .unwrap()
                .status,
            RunStatus::WaitingApproval
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

    /// 让出执行名额：状态与事件都落到位。
    #[tokio::test]
    async fn suspending_records_both_the_event_and_the_status() {
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
                Wait::Retry {
                    attempts: 2,
                    next_retry_at: later,
                    reason: "provider 超时".into(),
                },
            )
            .await
            .unwrap();

        let record = crate::repos::runs::get(&f.db, &accepted.run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.status, RunStatus::WaitingRetry);
        assert_eq!(record.retry_attempts, 2);
        assert_eq!(record.next_retry_at, Some(later));
        assert_eq!(
            types(&f.coordinator, &f.session).await.last().unwrap(),
            "run.waiting_retry"
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
}
