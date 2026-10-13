//! `Coordinator`：impl [`Ledger`]，正文 → JSONL → 数据库的唯一写入口（§8.5、§13.5）。
//!
//! **每个 Session 一个实例，串行。** 模型、工具、控制审计与 Memory 来源标记都经过它，
//! 不能各自追加导致行内容交错（§8.3）；串行由内部的 mutex 保证——一个 actor 在这里
//! 只会多一层消息，不会多一分保证。
//!
//! 每个方法内部完成 §8.5 的那一段箭头，顺序永远是：
//!
//! ```text
//! 外置正文（如有）→ JSONL 追加并 sync_all → state.db 事务
//! ```
//!
//! 只有 [`Ledger::append_audit`] 是反的（§8.5 的控制审计补写）：权威已经在数据库里，
//! JSONL 那一份是审计副本，**它无法反向改写数据库里的决定**。
//!
//! 这个模块只有**结构与共用件**：打开、正文外置、追加、提交。`impl Ledger` 那八个方法
//! 在 [`ledger`] 里，一个方法一段箭头。

mod ledger;

pub use ledger::title_for;

use std::sync::Arc;

use komo_kernel::events::EventPayload;
use komo_kernel::traits::{Clock, LedgerError, StoreError};
use komo_kernel::types::ids::{EventId, ExecutorId, RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::plan::ExecutionPlan;
use komo_kernel::types::refs::PayloadRef;
use komo_kernel::types::turn::ToolCallRequest;
use time::OffsetDateTime;
use toasty::Executor;

use crate::db::{BoxFuture, Db, store_to_ledger};
use crate::models::ToolCallRow;
use crate::payloads::PayloadStore;
use crate::repos::{derive, session};
use crate::session_log::{
    AppendedEvent, PendingEvent, SessionLog, SessionPaths, TailExpectation, TailRepair,
};
use crate::tool_output::FileToolOutputStore;

/// 一个 Session 的持久化协调入口。
pub struct Coordinator {
    db: Db,
    session: SessionId,
    paths: SessionPaths,
    payloads: PayloadStore,
    log: tokio::sync::Mutex<SessionLog>,
    clock: Arc<dyn Clock>,
    /// 本次启动身份，写进 `tool_attempts.executor`（§8.7）。
    executor: Option<ExecutorId>,
}

impl std::fmt::Debug for Coordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Coordinator")
            .field("session", &self.session)
            .field("root", &self.paths.root())
            .finish_non_exhaustive()
    }
}

impl Coordinator {
    /// 打开一个 Session 的协调入口。
    ///
    /// 打开时校验 JSONL 尾部：末尾半行隔离后截断，中间损坏 / 已提交范围缺失 / 哈希不
    /// 匹配一律报 [`LedgerError::Corrupt`]（§8.3）。
    pub async fn open(
        db: Db,
        sessions_root: impl AsRef<std::path::Path>,
        session: SessionId,
        origin: &str,
        clock: Arc<dyn Clock>,
    ) -> Result<Coordinator, LedgerError> {
        let paths = SessionPaths::new(&sessions_root, &session);
        let jsonl_path = format!("sessions/{session}/events.jsonl");

        let now = clock.now();
        let session_for_tx = session.clone();
        let origin = origin.to_string();
        db.with_write_retry(move |ex| {
            let (session, origin, jsonl_path) =
                (session_for_tx.clone(), origin.clone(), jsonl_path.clone());
            Box::pin(async move {
                session::ensure_in(ex, &session, &origin, &jsonl_path, now).await?;
                Ok(())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .map_err(store_to_ledger)?;

        let record = session::get(&db, &session).await.map_err(store_to_ledger)?;
        let expectation = TailExpectation {
            applied_seq: record.map(|r| r.applied_seq).unwrap_or(Seq::ZERO),
            digests: session::digests(&db, &session)
                .await
                .map_err(store_to_ledger)?,
        };

        let log = SessionLog::open(paths.clone(), session.clone(), &expectation).await?;
        Ok(Coordinator {
            db,
            session,
            payloads: PayloadStore::new(paths.clone()),
            paths,
            log: tokio::sync::Mutex::new(log),
            clock,
            executor: None,
        })
    }

    /// 记下本次启动身份。`tool_attempts` 上记的就是它——**不能仅凭一个 PID 判断是否为
    /// 原进程**（§8.7）。
    pub fn with_executor(mut self, executor: ExecutorId) -> Self {
        self.executor = Some(executor);
        self
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }

    pub fn paths(&self) -> &SessionPaths {
        &self.paths
    }

    pub fn payloads(&self) -> &PayloadStore {
        &self.payloads
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// 这个 Session 的工具输出存储。
    pub fn outputs(&self) -> FileToolOutputStore {
        FileToolOutputStore::new(self.paths.clone(), self.session.clone())
    }

    /// 打开时对 JSONL 尾部做了什么。
    pub async fn tail_repair(&self) -> TailRepair {
        self.log.lock().await.repair().clone()
    }

    /// 已经写到哪个 seq。
    pub async fn last_seq(&self) -> Seq {
        self.log.lock().await.last_seq()
    }

    /// 新事件正文完整内联；引用字段只用于读取旧日志。
    fn inline_text(text: &str) -> (Option<String>, Option<PayloadRef>) {
        (Some(text.to_string()), None)
    }

    /// 外置正文按引用读回来（§8.3：大正文存 `payloads/`；哈希校验由 [`PayloadStore::open`]
    /// 负责，不符就是 [`LedgerError::Corrupt`]，**不返回内容**）。
    ///
    /// 别的 Session 按目录推出来——与 `Ledger::read` 推日志路径是同一个形状（一个
    /// `Coordinator` 只握着自己那个 Session 的手边件，但读别人的是允许的）。
    async fn text_of(
        &self,
        session: &SessionId,
        reference: &PayloadRef,
    ) -> Result<String, LedgerError> {
        let payloads = if session == &self.session {
            self.payloads.clone()
        } else {
            PayloadStore::new(SessionPaths::at(
                self.paths
                    .root()
                    .parent()
                    .unwrap_or(self.paths.root())
                    .join(session.as_str()),
            ))
        };
        let bytes = payloads.open(reference).await.map_err(store_to_ledger)?;
        String::from_utf8(bytes)
            .map_err(|e| LedgerError::Corrupt(format!("外置正文不是 UTF-8：{e}")))
    }

    /// 新事件保留完整工具参数；`arguments_ref` 只用于读取旧日志。
    fn inline_arguments(calls: &[ToolCallRequest]) -> Vec<ToolCallRequest> {
        calls
            .iter()
            .cloned()
            .map(|call| ToolCallRequest {
                arguments_ref: if call.arguments.is_null() {
                    call.arguments_ref.clone()
                } else {
                    None
                },
                ..call
            })
            .collect()
    }

    /// 追加一条事件并同步。**返回之后才允许提交数据库事务**。
    async fn append(
        &self,
        run: Option<RunId>,
        payload: EventPayload,
    ) -> Result<AppendedEvent, LedgerError> {
        let now = self.clock.now();
        let mut log = self.log.lock().await;
        log.append(PendingEvent::new(EventId::new_at(now), run, now, payload))
            .await
    }

    /// 已经追加到这一条了：把事件索引与 `applied_seq` 一起提交。
    ///
    /// 「applied_seq 只能推进连续、已校验的事件前缀」（§8.5）——所以它跟在追加之后，
    /// 而不是和追加同时。
    async fn commit<F>(&self, appended: &AppendedEvent, extra: F) -> Result<(), LedgerError>
    where
        F: for<'a> Fn(
                &'a mut dyn Executor,
                AppendedEvent,
                OffsetDateTime,
            ) -> BoxFuture<'a, Result<(), StoreError>>
            + Send
            + Sync
            + 'static,
    {
        let now = self.clock.now();
        let session = self.session.clone();
        let appended = appended.clone();
        let bytes = self.log.lock().await.byte_len();
        // `extra` 进 `Arc`：`with_write_retry` 的闭包可能被调用多次，而借用闭包自己的
        // 环境会让引用逃出闭包——每次重跑克隆一个 `Arc` 是这里唯一写得出来的形状。
        let extra = Arc::new(extra);

        self.db
            .with_write_retry(move |ex| {
                let (session, appended, extra) = (session.clone(), appended.clone(), extra.clone());
                Box::pin(async move {
                    let seq = appended.seq();
                    session::index_event_in(&mut *ex, &appended).await?;
                    extra(&mut *ex, appended, now).await?;
                    session::advance_applied_in(&mut *ex, &session, seq, bytes, now).await?;
                    Ok(())
                }) as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
            .map_err(store_to_ledger)
    }

    /// 已经追加到这一条了：提交它的索引与**派生状态**。派生写入走 [`derive::apply_in`]
    /// ——与 backfill 是同一处投影，崩溃后补出来的行与此刻写下的逐列相同（§8.9）。
    ///
    /// `plan` 只在 `tool.planned` 外置到 `payloads/` 时要给；执行实例身份由这里带上。
    async fn commit_derived(
        &self,
        appended: &AppendedEvent,
        plan: Option<ExecutionPlan>,
    ) -> Result<(), LedgerError> {
        let session = self.session.clone();
        let executor = self.executor.clone();
        self.commit(appended, move |ex, appended, now| {
            let (session, plan, executor) = (session.clone(), plan.clone(), executor.clone());
            Box::pin(async move {
                let facts = derive::Facts {
                    plan: plan.as_ref(),
                    executor: executor.as_ref(),
                };
                derive::apply_in(ex, &session, &appended, facts, now).await
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
    }

    async fn run_of_call(&self, call: &ToolCallId) -> Result<ToolCallRow, LedgerError> {
        let id = call.to_string();
        self.db
            .read(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    ToolCallRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(crate::db::map_toasty)?
                        .ok_or_else(|| StoreError::NotFound {
                            what: format!("tool call {id}"),
                        })
                }) as BoxFuture<'_, Result<ToolCallRow, StoreError>>
            })
            .await
            .map_err(store_to_ledger)
    }
}
