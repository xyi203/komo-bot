//! 每个 Session 一个 `Coordinator`，Gateway 这一侧把它们接成三样东西。
//!
//! `Coordinator` 是**按 Session** 开的（store 的 §8.3 串行写入器），而 Agent Loop、
//! 调度器、恢复扫描与 Cron 各自只拿一个 `Arc<dyn Ledger>`。中间这一层就是把
//! 「Run / 调用 / 尝试」翻译成「哪个 Session」：
//!
//! ```text
//! SessionLedgers   SessionId → Arc<Coordinator>（缓存，开一次）
//!   └ PublishingLedger  每次写完把新事件推给 SSE（内存通知，丢了不丢数据）
//! RoutedLedger     Ledger 的全部方法 → 按 Run / 调用 / 尝试找到那个 Session
//! RoutedOutputs    ToolOutputStore：按尝试身份找 Session；`open` 从引用路径里的
//!                  run 认回来（引用形如 `tool-output/<run>/<call>/<attempt>/…`）
//! ```
//!
//! Run → Session 有账本可查（`runs.session_id`）；调用与尝试的对应关系**不在 store 的
//! 公开面上**，所以这里自己记一份：`record_round` 交出调用号时记，`start_call` 交出
//! 尝试号时记，恢复装配一段时把那个 Session 的日志过一遍补记（[`RoutedLedger::learn`]）。
//! 于是执行器可能用到的每一个调用号，要么是刚刚记下来的，要么是刚读过的。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::events::EventPayload;
use komo_kernel::traits::{Clock, Ledger, LedgerError, OutputWriter, StoreError, ToolOutputStore};
use komo_kernel::types::ids::{AttemptId, EventId, ExecutorId, RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::plan::{ExecutionPlan, PlanSource};
use komo_kernel::types::refs::{
    AttemptRef, OutputRef, PublishedOutput, ToolResultBody, VerifiedOutput,
};
use komo_kernel::types::status::{RunEnd, WaitReason};
use komo_kernel::types::turn::{AcceptInput, Accepted, AssistantRound, EventBatch, GrantUse};
use komo_store::{Coordinator, Db, FileToolOutputStore, SessionPaths};

use crate::sse::SharedHub;

/// 新建 Session 时记下的来源。只是元数据，`origin` 已经在的行不会被改写。
pub fn origin_of(source: &PlanSource) -> &'static str {
    match source {
        PlanSource::Interactive { .. } => "agent",
        PlanSource::Cron { .. } => "cron",
        PlanSource::Memory { .. } => "memory",
        PlanSource::Verification { .. } => "agent",
    }
}

/// 一个 Session 的三件套。
pub struct SessionEntry {
    pub coordinator: Arc<Coordinator>,
    pub ledger: Arc<PublishingLedger>,
    pub outputs: Arc<FileToolOutputStore>,
}

impl std::fmt::Debug for SessionEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionEntry")
            .field("session", self.coordinator.session())
            .finish()
    }
}

/// `SessionId → Coordinator` 的缓存。**同一个 Session 只能有一个写入器**，所以开的
/// 动作在一把锁里串起来。
pub struct SessionLedgers {
    db: Db,
    root: PathBuf,
    clock: Arc<dyn Clock>,
    hub: SharedHub,
    executor: ExecutorId,
    entries: tokio::sync::Mutex<BTreeMap<SessionId, Arc<SessionEntry>>>,
}

impl std::fmt::Debug for SessionLedgers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionLedgers")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl SessionLedgers {
    pub fn new(
        db: Db,
        root: impl Into<PathBuf>,
        clock: Arc<dyn Clock>,
        hub: SharedHub,
        executor: ExecutorId,
    ) -> Self {
        SessionLedgers {
            db,
            root: root.into(),
            clock,
            hub,
            executor,
            entries: tokio::sync::Mutex::new(BTreeMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 这个 Session 的目录路径。**只算路径，不碰磁盘**——`Ledger::read` 那条只读路径
    /// 从这里拿位置，再去 [`komo_store::session_log::read_events`] 读（§8.9：观察不改写）。
    pub fn paths_for(&self, session: &SessionId) -> SessionPaths {
        paths_for(&self.root, session)
    }

    /// 拿这个 Session 的三件套；没开过就开一个。
    ///
    /// **它会创建内容**：`Coordinator::open` → `SessionPaths::ensure` 建出会话目录、
    /// `SessionLog::open` 落下一份 `events.jsonl`。所以它是**写路径的入口**（accept /
    /// boundary / suspend / complete / append_audit）——只读的观察不能走它，否则"读一次"
    /// 就把被搬走的目录又建回来了（§8.9、§8.10：reconcile 正是靠"内容在不在"判一条 Run
    /// 能不能领）。
    pub async fn open(
        &self,
        session: &SessionId,
        origin: &str,
    ) -> Result<Arc<SessionEntry>, LedgerError> {
        let mut entries = self.entries.lock().await;
        if let Some(entry) = entries.get(session) {
            return Ok(Arc::clone(entry));
        }
        let coordinator = Coordinator::open(
            self.db.clone(),
            &self.root,
            session.clone(),
            origin,
            Arc::clone(&self.clock),
        )
        .await?
        .with_executor(self.executor.clone());
        let coordinator = Arc::new(coordinator);
        let outputs = Arc::new(coordinator.outputs());
        let last = coordinator.last_seq().await;
        let ledger = Arc::new(PublishingLedger::new(
            Arc::clone(&coordinator),
            session.clone(),
            Arc::clone(&self.hub),
            last,
        ));
        let entry = Arc::new(SessionEntry {
            coordinator,
            ledger,
            outputs,
        });
        entries.insert(session.clone(), Arc::clone(&entry));
        Ok(entry)
    }

    /// 已经开过的那些。
    pub async fn opened(&self) -> Vec<SessionId> {
        self.entries.lock().await.keys().cloned().collect()
    }
}

/// 写完一步就把新事件推给 SSE。
///
/// 推的是**已经同步且索引完成**的事件（`Ledger::read` 读回来的那些，§8.8），所以
/// 客户端收到的每一帧都能在账本里找到；广播丢了也只是晚一点知道。
pub struct PublishingLedger {
    inner: Arc<Coordinator>,
    session: SessionId,
    hub: SharedHub,
    published: tokio::sync::Mutex<Seq>,
}

impl std::fmt::Debug for PublishingLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublishingLedger")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl PublishingLedger {
    pub fn new(inner: Arc<Coordinator>, session: SessionId, hub: SharedHub, from: Seq) -> Self {
        PublishingLedger {
            inner,
            session,
            hub,
            published: tokio::sync::Mutex::new(from),
        }
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// 把还没推过的事件推出去。
    pub async fn pump(&self) {
        let mut published = self.published.lock().await;
        loop {
            let batch = match self.inner.read(&self.session, *published, 0).await {
                Ok(batch) => batch,
                Err(error) => {
                    tracing::warn!(session = %self.session, %error, "补读新事件失败，这次不推");
                    return;
                }
            };
            for event in &batch.events {
                *published = (*published).max(event.seq);
                self.hub.publish_event(event);
            }
            match batch.next {
                Some(_) if !batch.events.is_empty() => continue,
                _ => return,
            }
        }
    }
}

macro_rules! pumped {
    ($self:ident, $call:expr) => {{
        let out = $call;
        $self.pump().await;
        out
    }};
}

#[async_trait]
impl Ledger for PublishingLedger {
    async fn accept_input(&self, input: AcceptInput) -> Result<Accepted, LedgerError> {
        pumped!(self, self.inner.accept_input(input).await)
    }

    async fn record_round(
        &self,
        run: &RunId,
        round: AssistantRound,
    ) -> Result<Vec<ToolCallId>, LedgerError> {
        pumped!(self, self.inner.record_round(run, round).await)
    }

    async fn start_run(
        &self,
        run: &RunId,
        executor: &ExecutorId,
        generation: u64,
    ) -> Result<(), LedgerError> {
        pumped!(self, self.inner.start_run(run, executor, generation).await)
    }

    async fn plan_call(
        &self,
        call: &ToolCallId,
        plan: &ExecutionPlan,
    ) -> Result<EventId, LedgerError> {
        pumped!(self, self.inner.plan_call(call, plan).await)
    }

    async fn start_call(
        &self,
        call: &ToolCallId,
        plan: &ExecutionPlan,
        grant: Option<GrantUse>,
    ) -> Result<AttemptId, LedgerError> {
        pumped!(self, self.inner.start_call(call, plan, grant).await)
    }

    async fn finish_call(
        &self,
        attempt: &AttemptId,
        published: PublishedOutput,
    ) -> Result<(), LedgerError> {
        pumped!(self, self.inner.finish_call(attempt, published).await)
    }

    async fn suspend(&self, run: &RunId, wait: WaitReason) -> Result<(), LedgerError> {
        pumped!(self, self.inner.suspend(run, wait).await)
    }

    async fn complete(&self, run: &RunId, end: RunEnd) -> Result<(), LedgerError> {
        pumped!(self, self.inner.complete(run, end).await)
    }

    async fn read(
        &self,
        session: &SessionId,
        from: Seq,
        limit: u32,
    ) -> Result<EventBatch, LedgerError> {
        self.inner.read(session, from, limit).await
    }

    async fn boundary(&self, session: &SessionId) -> Result<Seq, LedgerError> {
        pumped!(self, self.inner.boundary(session).await)
    }

    async fn append_audit(
        &self,
        session: &SessionId,
        event_id: &EventId,
        payload: EventPayload,
        occurred_at: time::OffsetDateTime,
    ) -> Result<Seq, LedgerError> {
        pumped!(
            self,
            self.inner
                .append_audit(session, event_id, payload, occurred_at)
                .await
        )
    }
}

/// 跨 Session 的账本门面。
pub struct RoutedLedger {
    ledgers: Arc<SessionLedgers>,
    db: Db,
    runs: Mutex<BTreeMap<RunId, SessionId>>,
    calls: Mutex<BTreeMap<ToolCallId, SessionId>>,
    attempts: Mutex<BTreeMap<AttemptId, SessionId>>,
    /// 「有人刚停在一份**待审批**上」的信号（见 [`Self::suspend`]）。
    audit_wake: Arc<tokio::sync::Notify>,
}

impl std::fmt::Debug for RoutedLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutedLedger").finish_non_exhaustive()
    }
}

impl RoutedLedger {
    pub fn new(ledgers: Arc<SessionLedgers>, db: Db, audit_wake: Arc<tokio::sync::Notify>) -> Self {
        RoutedLedger {
            ledgers,
            db,
            runs: Mutex::new(BTreeMap::new()),
            calls: Mutex::new(BTreeMap::new()),
            attempts: Mutex::new(BTreeMap::new()),
            audit_wake,
        }
    }

    pub fn ledgers(&self) -> &Arc<SessionLedgers> {
        &self.ledgers
    }

    pub fn note_run(&self, run: &RunId, session: &SessionId) {
        self.runs
            .lock()
            .expect("run 表")
            .insert(run.clone(), session.clone());
    }

    fn note_call(&self, call: &ToolCallId, session: &SessionId) {
        self.calls
            .lock()
            .expect("调用表")
            .insert(call.clone(), session.clone());
    }

    fn note_attempt(&self, attempt: &AttemptId, session: &SessionId) {
        self.attempts
            .lock()
            .expect("尝试表")
            .insert(attempt.clone(), session.clone());
    }

    /// 这个 Run 属于哪个 Session。缓存没有就问账本（`runs.session_id`）。
    pub async fn session_of_run(&self, run: &RunId) -> Result<SessionId, LedgerError> {
        if let Some(session) = self.runs.lock().expect("run 表").get(run) {
            return Ok(session.clone());
        }
        let record = komo_store::repos::runs::get(&self.db, run)
            .await
            .map_err(komo_store::db::store_to_ledger)?
            .ok_or_else(|| LedgerError::NotFound {
                what: format!("run {run}"),
            })?;
        self.note_run(run, &record.session);
        Ok(record.session)
    }

    /// 把一个 Session 的调用与尝试补记进来。恢复装配一段之前调它。
    pub async fn learn(&self, session: &SessionId) -> Result<(), LedgerError> {
        let entry = self.ledgers.open(session, "agent").await?;
        let mut from = Seq::ZERO;
        loop {
            let batch = entry.ledger.read(session, from, 0).await?;
            if batch.events.is_empty() {
                return Ok(());
            }
            for event in &batch.events {
                from = from.max(event.seq);
                if let Some(run) = &event.run {
                    self.note_run(run, session);
                }
                match &event.payload {
                    EventPayload::MessageAssistant(body) => {
                        for call in &body.tool_calls {
                            self.note_call(&call.call_id, session);
                        }
                    }
                    EventPayload::ToolPlanned(body) => self.note_call(&body.call_id, session),
                    EventPayload::ToolStarted(body) => {
                        self.note_call(&body.call_id, session);
                        self.note_attempt(&body.attempt_id, session);
                    }
                    EventPayload::ToolResult(body) => {
                        self.note_call(&body.call_id, session);
                        self.note_attempt(&body.attempt_id, session);
                    }
                    _ => {}
                }
            }
            if batch.next.is_none() {
                return Ok(());
            }
        }
    }

    async fn for_run(&self, run: &RunId) -> Result<Arc<SessionEntry>, LedgerError> {
        let session = self.session_of_run(run).await?;
        self.ledgers.open(&session, "agent").await
    }

    fn known_call(&self, call: &ToolCallId) -> Option<SessionId> {
        self.calls.lock().expect("调用表").get(call).cloned()
    }

    fn known_attempt(&self, attempt: &AttemptId) -> Option<SessionId> {
        self.attempts.lock().expect("尝试表").get(attempt).cloned()
    }

    async fn for_call(&self, call: &ToolCallId) -> Result<Arc<SessionEntry>, LedgerError> {
        let session = match self.known_call(call) {
            Some(session) => session,
            None => {
                self.relearn().await?;
                self.known_call(call).ok_or_else(|| LedgerError::NotFound {
                    what: format!("调用 {call} 属于哪个会话"),
                })?
            }
        };
        self.ledgers.open(&session, "agent").await
    }

    async fn for_attempt(&self, attempt: &AttemptId) -> Result<Arc<SessionEntry>, LedgerError> {
        let session = match self.known_attempt(attempt) {
            Some(session) => session,
            None => {
                self.relearn().await?;
                self.known_attempt(attempt)
                    .ok_or_else(|| LedgerError::NotFound {
                        what: format!("尝试 {attempt} 属于哪个会话"),
                    })?
            }
        };
        self.ledgers.open(&session, "agent").await
    }

    /// 冷进程里的兜底：把**还没终态的那些 Run** 所在会话的日志过一遍，把调用与尝试补记
    /// 进来。
    ///
    /// 恢复扫描补记 `finish_call` 走的正是这条路：尝试号是从磁盘上的孤儿 `output.json`
    /// 与日志里读出来的，进程内的表还是空的（§8.4 第 7/8 行）。
    ///
    // TODO(decide: 真正该有的是 store 的 `repos::calls::session_of_{call,attempt}`——
    // `tool_calls` / `tool_attempts` 上本来就有 `session_id` 列，一次按主键的查询就够。
    // 那两个函数还没有（store 的交付报告里列过），所以这里先按"未完成 Run 的会话"补学
    // 一遍：数量是一次启动里未完成的任务数，且学过就缓存，不是每次调用都扫。)
    async fn relearn(&self) -> Result<(), LedgerError> {
        let sessions = {
            let mut sessions: Vec<SessionId> = komo_store::repos::runs::unfinished(&self.db)
                .await
                .map_err(komo_store::db::store_to_ledger)?
                .into_iter()
                .map(|record| record.session)
                .collect();
            sessions.sort();
            sessions.dedup();
            sessions
        };
        for session in sessions {
            self.learn(&session).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl Ledger for RoutedLedger {
    async fn accept_input(&self, input: AcceptInput) -> Result<Accepted, LedgerError> {
        let session = input.session.clone();
        let entry = self
            .ledgers
            .open(&session, origin_of(&input.source))
            .await?;
        let accepted = entry.ledger.accept_input(input).await?;
        self.note_run(&accepted.run, &session);
        Ok(accepted)
    }

    async fn record_round(
        &self,
        run: &RunId,
        round: AssistantRound,
    ) -> Result<Vec<ToolCallId>, LedgerError> {
        let session = self.session_of_run(run).await?;
        let entry = self.ledgers.open(&session, "agent").await?;
        let calls = entry.ledger.record_round(run, round).await?;
        for call in &calls {
            self.note_call(call, &session);
        }
        Ok(calls)
    }

    async fn start_run(
        &self,
        run: &RunId,
        executor: &ExecutorId,
        generation: u64,
    ) -> Result<(), LedgerError> {
        self.for_run(run)
            .await?
            .ledger
            .start_run(run, executor, generation)
            .await
    }

    async fn plan_call(
        &self,
        call: &ToolCallId,
        plan: &ExecutionPlan,
    ) -> Result<EventId, LedgerError> {
        self.for_call(call)
            .await?
            .ledger
            .plan_call(call, plan)
            .await
    }

    async fn start_call(
        &self,
        call: &ToolCallId,
        plan: &ExecutionPlan,
        grant: Option<GrantUse>,
    ) -> Result<AttemptId, LedgerError> {
        let entry = self.for_call(call).await?;
        let attempt = entry.ledger.start_call(call, plan, grant).await?;
        self.note_attempt(&attempt, entry.ledger.session());
        Ok(attempt)
    }

    async fn finish_call(
        &self,
        attempt: &AttemptId,
        published: PublishedOutput,
    ) -> Result<(), LedgerError> {
        self.for_attempt(attempt)
            .await?
            .ledger
            .finish_call(attempt, published)
            .await
    }

    async fn suspend(&self, run: &RunId, wait: WaitReason) -> Result<(), LedgerError> {
        let outcome = self
            .for_run(run)
            .await?
            .ledger
            .suspend(run, wait.clone())
            .await;
        // 停在**待审批**上：`approval.requested` 那条审计事件还在 `control_outbox` 里
        // （§8.5 的反向顺序：state.db 权威先提交，JSONL 那一条随后补写），而界面正是靠
        // 它才知道有人等着回答——`run.waiting` 只说"停在审批上"，短 ID 在它身上。
        // 等周期（`AUDIT_TICK`）就是让操作者的弹窗晚到一分钟，所以这里立刻叫醒补写。
        //
        // 审批请求此刻**已经提交**（`ApprovalGate::request` 在 `suspend` 之前跑完），
        // 所以醒来一定能读到那一行；拿不到时补写留到下一拍，不影响权威。
        if outcome.is_ok() && matches!(wait, WaitReason::Approval { .. }) {
            self.audit_wake.notify_one();
        }
        outcome
    }

    async fn complete(&self, run: &RunId, end: RunEnd) -> Result<(), LedgerError> {
        self.for_run(run).await?.ledger.complete(run, end).await
    }

    async fn read(
        &self,
        session: &SessionId,
        from: Seq,
        limit: u32,
    ) -> Result<EventBatch, LedgerError> {
        // **读不得创建或修改内容**（§8.9：观察不改写）。所以这里不走
        // `SessionLedgers::open`——那条路会 `ensure` 建目录、落一份日志，读一次就把搬走
        // 的会话目录又建回来了，而 §8.10 判一条 Run 能不能领靠的正是"内容在不在"。
        // 直接按路径读磁盘：目录/日志不在就是"读不出来"，不替它造一份空的。
        let paths = self.ledgers.paths_for(session);
        let (events, more) =
            komo_store::session_log::read_events(&paths, session, from, limit).await?;
        let next = more.then(|| events.last().map(|event| event.seq)).flatten();
        Ok(EventBatch {
            session: session.clone(),
            events,
            next,
        })
    }

    async fn boundary(&self, session: &SessionId) -> Result<Seq, LedgerError> {
        self.ledgers
            .open(session, "agent")
            .await?
            .ledger
            .boundary(session)
            .await
    }

    async fn append_audit(
        &self,
        session: &SessionId,
        event_id: &EventId,
        payload: EventPayload,
        occurred_at: time::OffsetDateTime,
    ) -> Result<Seq, LedgerError> {
        self.ledgers
            .open(session, "agent")
            .await?
            .ledger
            .append_audit(session, event_id, payload, occurred_at)
            .await
    }
}

/// 跨 Session 的工具输出存储。
///
/// `begin` / `publish` 有 [`AttemptRef`]，里面就带着 Session；`open` 只有一个引用，
/// 而引用是**相对 Session 目录**的（`tool-output/<run>/<call>/<attempt>/output.json`）
/// ——所以从路径里认回 run，再问账本它属于哪个 Session。认不出来就报
/// [`StoreError::Corrupt`]：读不出来 ≠ 确认没问题（§8.5）。
pub struct RoutedOutputs {
    ledgers: Arc<SessionLedgers>,
    routed: Arc<RoutedLedger>,
}

impl std::fmt::Debug for RoutedOutputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutedOutputs").finish_non_exhaustive()
    }
}

impl RoutedOutputs {
    pub fn new(ledgers: Arc<SessionLedgers>, routed: Arc<RoutedLedger>) -> Self {
        RoutedOutputs { ledgers, routed }
    }

    async fn store_for(&self, session: &SessionId) -> Result<Arc<FileToolOutputStore>, StoreError> {
        self.ledgers
            .open(session, "agent")
            .await
            .map(|entry| Arc::clone(&entry.outputs))
            .map_err(|error| StoreError::Other(error.to_string()))
    }
}

/// 从 `tool-output/<run>/…` 里认出 run。
fn run_in(reference: &OutputRef) -> Option<RunId> {
    let mut parts = reference.path().split('/');
    if parts.next()? != "tool-output" {
        return None;
    }
    parts.next().map(RunId::from_raw)
}

#[async_trait]
impl ToolOutputStore for RoutedOutputs {
    async fn begin(&self, attempt: &AttemptRef) -> Result<Box<dyn OutputWriter>, StoreError> {
        self.store_for(&attempt.session).await?.begin(attempt).await
    }

    async fn publish(
        &self,
        writer: Box<dyn OutputWriter>,
        result: ToolResultBody,
    ) -> Result<PublishedOutput, StoreError> {
        let session = writer.attempt().session.clone();
        self.store_for(&session)
            .await?
            .publish(writer, result)
            .await
    }

    async fn open(&self, output: &OutputRef) -> Result<VerifiedOutput, StoreError> {
        let run = run_in(output).ok_or_else(|| {
            StoreError::Corrupt(format!("输出引用认不出它属于哪个运行：{}", output.path()))
        })?;
        let session = self
            .routed
            .session_of_run(&run)
            .await
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        self.store_for(&session).await?.open(output).await
    }
}

/// 一个 Session 的目录。
pub fn paths_for(root: &Path, session: &SessionId) -> SessionPaths {
    SessionPaths::new(root, session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::test_support::{TestClock, sample_model};
    use komo_kernel::types::ids::RequestKey;
    use komo_store::test_support::TempStore;

    async fn ledgers(store: &TempStore) -> (Arc<SessionLedgers>, Arc<RoutedLedger>, SharedHub) {
        let hub: SharedHub = Arc::new(crate::sse::EventHub::new());
        let ledgers = Arc::new(SessionLedgers::new(
            store.db().clone(),
            store.sessions_root(),
            Arc::new(TestClock::fixed()),
            Arc::clone(&hub),
            ExecutorId::from_raw("exec-1"),
        ));
        let routed = Arc::new(RoutedLedger::new(
            Arc::clone(&ledgers),
            store.db().clone(),
            Arc::new(tokio::sync::Notify::new()),
        ));
        (ledgers, routed, hub)
    }

    fn input(session: &SessionId, key: &str) -> AcceptInput {
        AcceptInput {
            session: session.clone(),
            request_key: RequestKey::new(key),
            text: "在吗".into(),
            source: PlanSource::Interactive {
                session: session.clone(),
            },
            peer: None,
            model: sample_model(),
            workdir: None,
            at: time::macros::datetime!(2026-09-16 08:00:00 UTC),
        }
    }

    #[tokio::test]
    async fn opening_the_same_session_twice_reuses_one_writer() {
        let store = TempStore::open().await.unwrap();
        let (ledgers, _routed, _hub) = ledgers(&store).await;
        let session = SessionId::from_raw("sess-1");
        let first = ledgers.open(&session, "agent").await.unwrap();
        let second = ledgers.open(&session, "agent").await.unwrap();
        assert!(Arc::ptr_eq(&first.coordinator, &second.coordinator));
    }

    #[tokio::test]
    async fn a_routed_write_reaches_the_session_that_owns_the_run() {
        let store = TempStore::open().await.unwrap();
        let (_ledgers, routed, hub) = ledgers(&store).await;
        let session = SessionId::from_raw("sess-1");
        let mut sub = hub.subscribe(&session);

        let accepted = routed.accept_input(input(&session, "k-1")).await.unwrap();
        assert_eq!(routed.session_of_run(&accepted.run).await.unwrap(), session);

        // 接受输入之后 SSE 上就有事件了——广播是内存通知，但它确实发生了。
        let frame = sub.recv().await.expect("有事件");
        assert_eq!(frame.session, session);

        // 不认识的 Run 报找不到，而不是随便挑一个会话。
        let error = routed
            .session_of_run(&RunId::from_raw("run-nope"))
            .await
            .unwrap_err();
        assert!(matches!(error, LedgerError::NotFound { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_run_to_session_mapping_survives_a_cold_cache() {
        let store = TempStore::open().await.unwrap();
        let session = SessionId::from_raw("sess-1");
        let run = {
            let (_ledgers, routed, _hub) = ledgers(&store).await;
            routed
                .accept_input(input(&session, "k-1"))
                .await
                .unwrap()
                .run
        };
        // 新的一层（相当于重启后的进程）：缓存是空的，只能从账本认回来。
        let (_ledgers, routed, _hub) = ledgers(&store).await;
        assert_eq!(routed.session_of_run(&run).await.unwrap(), session);
    }

    #[test]
    fn an_output_reference_names_its_run() {
        let reference = OutputRef(komo_kernel::types::refs::ContentRef {
            path: "tool-output/run-1/call-1/attempt-1/output.json".into(),
            size: 0,
            hash: komo_kernel::types::digest::ContentHash::of_str(""),
            pointer: None,
        });
        assert_eq!(run_in(&reference), Some(RunId::from_raw("run-1")));
    }
}
