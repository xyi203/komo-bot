//! 任务调度器（spec §5.1、§5.4、§5.5；pi `harness/scheduler.ts` 的移植）。
//!
//! 判定都在 `komo_kernel::durable::task`（纯函数）；这里只做三件事：把已提交的记录喂进
//! 所有权树、在线上写决定、在线外跑 phase handler。
//!
//! 不变量（同 pi）：每次任务状态变化都由线上串行的一个闭包决定并写入——reservation、abort
//! 标记、runtime 提交、收尾，以及每个阶段前的 step。handler 与 join 在线外。invocation 在
//! 决定它结束的那个 step 里结束，所以它排队的 runtime 提交要么先于这个决定落地，要么被拒。
//!
//! 与 pi 的差别：所有权索引不做"只保留活工作周围"的增量裁剪（节点与边只增不删），也没有
//! `runnable` / `finalizeChecks` / `failFastChecks` 之类的候选集——每一轮 reconcile 与
//! reservation 都直接扫描活任务（体量按活任务数计，没有性能问题）；因此也没有
//! `cascadePending`：reconcile 每次都从已提交记录重新推出该打的全部标记。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures_util::FutureExt;
use serde_json::Value;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use komo_kernel::durable::records::{AbortReason, JoinPolicy, TaskOutcome, TaskOutcomeError};
use komo_kernel::durable::task::graph::Scope;
use komo_kernel::durable::task::{
    BlockedReason, Fit, InvocationMode, NextTaskState, Node, Overlay, PhaseReturn, Replacement,
    RunningTask, StepDecision, StepInput, TaskGraph, TaskNode, abort_marks, can_reserve,
    check_checkpoint, check_next_state, check_replacement, decide_step, failed_outcome,
    finalizable, fit, is_live, reconcile_on_open, waiting_on, with_abort_mark, with_state,
};
use komo_kernel::durable::{
    ConversationId, Storage, StorageWrite, TaskId, TaskRecord, TaskState, TaskStatus,
};

use super::error::DurableError;
use super::kind::{Registry, RegistrySnapshot, Reporter, TaskHooks, TaskKind, identity};
use super::line::{Line, Publication, TxFuture};
use super::runtime::TaskRuntime;
use super::tx::{TaskScope, Tx};

/// 一次 invocation：按 run 或 abort 模式跑任务代码的内存里的一段执行。
pub(super) struct Invocation {
    pub task: TaskId,
    pub conversation: ConversationId,
    pub mode: InvocationMode,
    /// 信号：abort 标记、关闭、结束时取消；handler 里的等待用它退出。
    pub token: CancellationToken,
    ended: AtomicBool,
    started: AtomicBool,
    done: watch::Sender<bool>,
}

impl Invocation {
    fn new(record: &TaskRecord, mode: InvocationMode) -> Arc<Self> {
        let (done, _) = watch::channel(false);
        Arc::new(Self {
            task: record.id,
            conversation: record.conversation,
            mode,
            token: CancellationToken::new(),
            ended: AtomicBool::new(false),
            started: AtomicBool::new(false),
            done,
        })
    }

    pub fn is_ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> Result<(), DurableError> {
        if self.is_ended() {
            Err(DurableError::InvocationEnded(self.task))
        } else {
            Ok(())
        }
    }

    fn finish(&self) {
        self.done.send_replace(true);
    }

    async fn wait_done(&self) {
        let mut rx = self.done.subscribe();
        let _ = rx.wait_for(|done| *done).await;
    }
}

struct Reservation {
    invocation: Arc<Invocation>,
    kind: Arc<dyn TaskKind>,
}

#[allow(clippy::large_enum_variant)]
enum Resolution {
    Ready {
        kind: Arc<dyn TaskKind>,
        record: TaskRecord,
    },
    Blocked(BlockedReason),
}

/// 调度器眼里一条活任务现在的样子（派生，不落盘）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskInspectionState {
    Running,
    Completing,
    Waiting { on: Vec<TaskId> },
    Blocked { reason: BlockedReason },
    Ready { migrates: bool },
}

#[derive(Debug, Clone)]
pub struct TaskInspection {
    pub record: TaskRecord,
    pub state: TaskInspectionState,
}

type TaskWaiter = oneshot::Sender<Result<TaskRecord, DurableError>>;

struct IdleWaiter {
    conversation: Option<ConversationId>,
    tx: oneshot::Sender<Result<(), DurableError>>,
}

#[derive(Default)]
pub(super) struct State {
    pub graph: TaskGraph,
    pub invocations: HashMap<TaskId, Arc<Invocation>>,
    /// 链还没读全的活任务。
    unloaded: BTreeSet<TaskId>,
    /// 迁移失败过的任务 → 当时的定义对象；换了定义才再试。
    failed_migrations: HashMap<TaskId, usize>,
    /// 开库时发现的 `abandon_on_restart` 活任务：第一轮 reservation 只给它们打 restart 标记。
    abandoned: BTreeSet<TaskId>,
    task_waiters: HashMap<TaskId, Vec<TaskWaiter>>,
    idle_waiters: Vec<IdleWaiter>,
    reconcile_scheduled: bool,
    pub enabled: bool,
    pub closing: bool,
    dirty: bool,
    draining: bool,
    registry_sub: Option<u64>,
}

impl State {
    fn track(&mut self, record: &TaskRecord) {
        self.graph.track(record);
        if is_live(record) {
            if self
                .graph
                .missing_above(TaskNode::of(record).parent())
                .is_some()
            {
                self.unloaded.insert(record.id);
            }
        } else {
            self.unloaded.remove(&record.id);
        }
    }
}

pub(super) struct Sched {
    pub line: Line,
    pub storage: Arc<dyn Storage>,
    pub registry: Registry,
    pub report: Reporter,
    hooks: Arc<dyn TaskHooks>,
    state: Mutex<State>,
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = panic.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = panic.downcast_ref::<String>() {
        text.clone()
    } else {
        "panic".to_owned()
    }
}

fn state_checkpoint(record: &TaskRecord) -> Option<&Value> {
    match &record.state {
        TaskState::Pending { checkpoint }
        | TaskState::Running { checkpoint }
        | TaskState::Waiting { checkpoint, .. } => Some(checkpoint),
        _ => None,
    }
}

fn set_checkpoint(record: &mut TaskRecord, new: Value) {
    match &mut record.state {
        TaskState::Pending { checkpoint }
        | TaskState::Running { checkpoint }
        | TaskState::Waiting { checkpoint, .. } => *checkpoint = new,
        _ => {}
    }
}

impl Sched {
    pub fn new(
        line: Line,
        registry: Registry,
        report: Reporter,
        hooks: Arc<dyn TaskHooks>,
    ) -> Arc<Self> {
        let storage = line.storage().clone();
        Arc::new(Self {
            line,
            storage,
            registry,
            report,
            hooks,
            state: Mutex::new(State::default()),
        })
    }

    pub fn st(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn report_text(&self, text: &str) {
        tracing::warn!("{text}");
        (self.report)(text);
    }

    // ------------------------------------------------------------------ 开关

    /// 读出活任务、把幸存的 `running` 改回 `pending`；不调度任何东西。
    pub async fn open(self: &Arc<Self>) -> Result<(), DurableError> {
        let weak = Arc::downgrade(self);
        self.line.subscribe_commits({
            let weak = weak.clone();
            move |publication| {
                if let Some(sched) = weak.upgrade() {
                    sched.observe(publication);
                }
            }
        });
        self.line.subscribe_close({
            let weak = weak.clone();
            move || {
                if let Some(sched) = weak.upgrade() {
                    sched.seal();
                }
            }
        });
        let sub = self.registry.subscribe({
            let weak = weak.clone();
            move || {
                if let Some(sched) = weak.upgrade() {
                    sched.kick();
                }
            }
        });
        self.st().registry_sub = Some(sub);
        let sched = self.clone();
        self.line
            .commit(None, move |tx| {
                Box::pin(async move {
                    // 所有表读都在第一次写之前。
                    let mut scans = Vec::new();
                    for status in [
                        TaskStatus::Pending,
                        TaskStatus::Running,
                        TaskStatus::Waiting,
                        TaskStatus::Completing,
                    ] {
                        scans.push(tx.all_tasks(status).await?);
                    }
                    for record in scans.into_iter().flatten() {
                        {
                            let mut st = sched.st();
                            st.track(&record);
                            if record.abandon_on_restart {
                                st.abandoned.insert(record.id);
                            }
                        }
                        if let Some(pending) = reconcile_on_open(&record) {
                            sched.write_task(tx, pending).await?;
                        }
                    }
                    Ok(())
                })
            })
            .await?;
        // 崩溃前没来得及落的取消级联、没收尾的 `completing`，都由 reconcile 重新推出。
        self.schedule_reconcile();
        Ok(())
    }

    pub fn resume(self: &Arc<Self>) {
        self.st().enabled = true;
        self.kick();
    }

    /// 封口监听：关闭开始（或失败）时同步调用。
    fn seal(&self) {
        let (task_waiters, idle_waiters, sub) = {
            let mut st = self.st();
            st.closing = true;
            (
                std::mem::take(&mut st.task_waiters),
                std::mem::take(&mut st.idle_waiters),
                st.registry_sub.take(),
            )
        };
        if let Some(sub) = sub {
            self.registry.unsubscribe(sub);
        }
        let error = self.line.closed_error();
        for waiter in task_waiters.into_values().flatten() {
            let _ = waiter.send(Err(error.clone()));
        }
        for waiter in idle_waiters {
            let _ = waiter.tx.send(Err(error.clone()));
        }
    }

    /// 给所有 invocation 发信号并等它们走完；不写任何结局。关闭时在所有 close 监听之后调用。
    pub async fn join(&self) {
        let invocations: Vec<Arc<Invocation>> = self.st().invocations.values().cloned().collect();
        for invocation in &invocations {
            invocation.token.cancel();
        }
        for invocation in invocations {
            invocation.wait_done().await;
        }
    }

    fn fail_session(&self, error: DurableError) {
        if !self.st().closing && !error.is_shutdown() {
            self.line.fail(error);
        }
    }

    // ------------------------------------------------------------------ 发布

    /// 提交监听（在线上、同步）：把已提交的记录喂进内存镜像，叫醒等待者与各轮扫描。
    fn observe(self: &Arc<Self>, publication: &Publication) {
        let mut resolved: Vec<(Vec<TaskWaiter>, TaskRecord)> = Vec::new();
        let mut signals: Vec<CancellationToken> = Vec::new();
        let mut changed = false;
        {
            let mut st = self.st();
            // 先记边，这样同一笔提交里在新 conversation 里建的任务能挂上去。
            for write in &publication.writes {
                if let StorageWrite::Conversation(record) = write
                    && st.graph.edge(record.id).is_none()
                {
                    st.graph
                        .set_edge(record.id, record.owner.map(|owner| owner.task));
                }
            }
            for write in &publication.writes {
                let StorageWrite::Task(record) = write else {
                    continue;
                };
                changed = true;
                let previous = st.graph.live_record(record.id).cloned();
                st.track(record);
                if !is_live(record) {
                    st.failed_migrations.remove(&record.id);
                    if let Some(waiters) = st.task_waiters.remove(&record.id) {
                        resolved.push((waiters, record.clone()));
                    }
                    continue;
                }
                let newly_marked =
                    record.abort_requested && !previous.is_some_and(|p| p.abort_requested);
                if newly_marked
                    && let Some(invocation) = st.invocations.get(&record.id)
                    && invocation.mode == InvocationMode::Run
                {
                    // 给 run invocation 发信号；它的下一个 step 会结束它。
                    signals.push(invocation.token.clone());
                }
            }
        }
        for (waiters, record) in resolved {
            for waiter in waiters {
                let _ = waiter.send(Ok(record.clone()));
            }
        }
        for token in signals {
            token.cancel();
        }
        if changed {
            self.schedule_reconcile();
            self.settle_idle();
            self.kick();
        }
    }

    fn settle_idle(&self) {
        let ready: Vec<IdleWaiter> = {
            let mut st = self.st();
            let mut ready = Vec::new();
            let mut index = 0;
            while index < st.idle_waiters.len() {
                if Self::idle_in(&st, st.idle_waiters[index].conversation) {
                    ready.push(st.idle_waiters.remove(index));
                } else {
                    index += 1;
                }
            }
            ready
        };
        for waiter in ready {
            let _ = waiter.tx.send(Ok(()));
        }
    }

    /// 范围内没有活的非后台任务；链没读全的任务算在里面，除非往上走能判定它在外面。
    fn idle_in(st: &State, conversation: Option<ConversationId>) -> bool {
        let scope = match conversation {
            Some(id) => Scope::Conversation(id),
            None => Scope::Roots,
        };
        for id in &st.unloaded {
            if let Some(record) = st.graph.live_record(*id)
                && !record.background
                && st
                    .graph
                    .in_scope(TaskNode::of(record).parent(), scope, false)
                    != Some(false)
            {
                return false;
            }
        }
        let starts: Vec<ConversationId> = match conversation {
            Some(id) => vec![id],
            None => st.graph.root_conversations(),
        };
        for start in starts {
            let walked = st
                .graph
                .walk_down(Node::Conversation(start), None, false, &|_| true);
            if walked
                .iter()
                .any(|walked| walked.live.is_some() && !walked.node.background)
            {
                return false;
            }
        }
        true
    }

    // ------------------------------------------------------------------ 写任务

    /// 暂存一个任务整条替换：状态边合法、归属字段不变；terminal 时随之退役任务名下的
    /// document（spec §5.3）。
    pub async fn write_task(&self, tx: &mut Tx, record: TaskRecord) -> Result<(), DurableError> {
        let old = self.st().graph.live_record(record.id).cloned();
        if let Some(old) = old
            && !tx.is_staged(record.id)
        {
            check_replacement(&old, &record)?;
        }
        let terminal = matches!(record.state, TaskState::Terminal { .. });
        let id = record.id;
        tx.set_task(record)?;
        if terminal {
            tx.retire_task_documents(id).await?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------ 所有权加载

    /// 读全 `unloaded` 里每个任务的所有者链（读已提交的存储，可在提交闭包里调用）。
    async fn load_scopes(&self) -> Result<(), DurableError> {
        let pending: Vec<TaskId> = self.st().unloaded.iter().copied().collect();
        for id in pending {
            let start = self.st().graph.node(id).map(|node| node.parent());
            if let Some(start) = start {
                self.load_chain(start, None).await?;
            }
            self.st().unloaded.remove(&id);
        }
        Ok(())
    }

    /// 从 `start` 往上把缺的任务节点与 conversation 边读进来；本次提交新建的节点从候选里取。
    async fn load_chain(&self, start: Node, overlay: Option<&Overlay>) -> Result<(), DurableError> {
        let mut at = start;
        loop {
            let missing = self.st().graph.missing_above(at);
            match missing {
                None => return Ok(()),
                Some(Node::Task(id)) => {
                    if let Some(record) = overlay.and_then(|o| o.tasks.get(&id)) {
                        at = TaskNode::of(record).parent();
                        continue;
                    }
                    let Some(record) = self.storage.task(id).await? else {
                        return Ok(());
                    };
                    self.st().graph.know(id, TaskNode::of(&record));
                }
                Some(Node::Conversation(id)) => {
                    if let Some(edge) = overlay.and_then(|o| o.edges.get(&id)) {
                        match edge {
                            Some(owner) => at = Node::Task(*owner),
                            None => return Ok(()),
                        }
                        continue;
                    }
                    let Some(record) = self.storage.conversation(id).await? else {
                        return Ok(());
                    };
                    self.st()
                        .graph
                        .set_edge(id, record.owner.map(|owner| owner.task));
                }
            }
        }
    }

    // ------------------------------------------------------------------ 定义解析

    fn resolve(&self, record: &TaskRecord, snapshot: &RegistrySnapshot) -> Resolution {
        let kind = snapshot.task(&record.kind);
        let failed_with_this = kind.as_ref().is_some_and(|kind| {
            self.st().failed_migrations.get(&record.id) == Some(&identity(kind))
        });
        match fit(
            record.version,
            kind.as_ref().map(|k| k.info()),
            failed_with_this,
        ) {
            Fit::Blocked(reason) => Resolution::Blocked(reason),
            Fit::Ready { migrates: false } => Resolution::Ready {
                kind: kind.expect("a ready fit has a definition"),
                record: record.clone(),
            },
            Fit::Ready { migrates: true } => {
                let kind = kind.expect("a ready fit has a definition");
                let checkpoint = state_checkpoint(record).cloned().unwrap_or(Value::Null);
                let attempt = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    kind.migrate(&record.input, &checkpoint, record.version)
                }));
                let failure = match attempt {
                    Ok(Some(Ok(migrated))) => match check_checkpoint(&migrated.checkpoint) {
                        Ok(()) => {
                            let mut next = record.clone();
                            next.version = kind.version();
                            next.input = migrated.input;
                            set_checkpoint(&mut next, migrated.checkpoint);
                            return Resolution::Ready { kind, record: next };
                        }
                        Err(error) => error.to_string(),
                    },
                    Ok(Some(Err(error))) => error.to_string(),
                    Ok(None) => format!(
                        "Task {} version {} has no migration from {}",
                        record.kind,
                        kind.version(),
                        record.version
                    ),
                    Err(panic) => panic_message(panic),
                };
                self.st()
                    .failed_migrations
                    .insert(record.id, identity(&kind));
                self.report_text(&format!(
                    "migration of task {} ({}) failed: {failure}",
                    record.id, record.kind
                ));
                Resolution::Blocked(BlockedReason::MigrationFailed)
            }
        }
    }

    // ------------------------------------------------------------------ 调度

    fn kick(self: &Arc<Self>) {
        {
            let mut st = self.st();
            st.dirty = true;
            if st.draining || !st.enabled || st.closing {
                return;
            }
            st.draining = true;
        }
        // 绝不在提交 / 注册表监听里同步提交。
        let sched = self.clone();
        tokio::spawn(async move { sched.drain().await });
    }

    async fn drain(self: Arc<Self>) {
        loop {
            {
                let mut st = self.st();
                if !(st.dirty && st.enabled && !st.closing) {
                    break;
                }
                st.dirty = false;
            }
            match self.reserve().await {
                Ok(reservations) => {
                    for reservation in reservations {
                        self.start(reservation);
                    }
                }
                Err(error) => {
                    // reservation 提交不跑任务代码：出错是存储故障或 bug，重跑一遍也修不好。
                    self.fail_session(error);
                    break;
                }
            }
        }
        let again = {
            let mut st = self.st();
            st.draining = false;
            st.dirty && st.enabled && !st.closing
        };
        if again {
            self.kick();
        }
    }

    /// 一次提交里 reserve 所有可跑的任务；被 abort 标记、又没有定义能接的任务在这里判
    /// `orphaned`（重启放弃的除外）。开库后的第一轮只给被放弃的任务打 restart 标记。
    async fn reserve(self: &Arc<Self>) -> Result<Vec<Reservation>, DurableError> {
        let sched = self.clone();
        let result = self
            .line
            .commit(None, move |tx| {
                Box::pin(async move {
                    let mut reservations = Vec::new();
                    let mut abandoned = Vec::new();
                    let (enabled, closing) = {
                        let st = sched.st();
                        (st.enabled, st.closing)
                    };
                    if !enabled || closing {
                        return Ok((reservations, abandoned));
                    }
                    let to_mark: Vec<TaskRecord> = {
                        let st = sched.st();
                        st.abandoned
                            .iter()
                            .filter_map(|id| st.graph.live_record(*id))
                            .filter(|record| !record.abort_requested)
                            .cloned()
                            .collect()
                    };
                    for record in to_mark {
                        sched
                            .write_task(tx, with_abort_mark(&record, Some(AbortReason::Restart)))
                            .await?;
                    }
                    {
                        let st = sched.st();
                        if !st.abandoned.is_empty() {
                            abandoned = st.abandoned.iter().copied().collect();
                            return Ok((reservations, abandoned));
                        }
                    }
                    sched.load_scopes().await?;
                    let snapshot = sched.registry.snapshot();
                    let candidates: Vec<TaskRecord> = {
                        let st = sched.st();
                        st.graph
                            .live()
                            .filter(|record| record.state.status() != TaskStatus::Completing)
                            .filter(|record| !st.invocations.contains_key(&record.id))
                            .cloned()
                            .collect()
                    };
                    for record in candidates {
                        // 还在等的（`on` 里有活的 / 有标记时名下有活的工作）：回来之前不跑。
                        let (waiting, cancelled) = {
                            let st = sched.st();
                            (
                                !waiting_on(&st.graph, &record, true).is_empty(),
                                !record.background
                                    && st
                                        .graph
                                        .cancelling_owner(TaskNode::of(&record).parent())
                                        .is_some(),
                            )
                        };
                        if waiting {
                            continue;
                        }
                        let mode = if record.abort_requested {
                            InvocationMode::Abort
                        } else {
                            InvocationMode::Run
                        };
                        // 取消意图所有者之下的工作不起新阶段，等级联给它打标记。
                        if mode == InvocationMode::Run && cancelled {
                            sched.schedule_reconcile();
                            continue;
                        }
                        match sched.resolve(&record, &snapshot) {
                            Resolution::Blocked(reason) => {
                                // 重启放弃的任务等定义回来，好让它的 abort handler 收尾。
                                if mode == InvocationMode::Abort && record.abort_reason.is_none() {
                                    sched
                                        .terminate(
                                            tx,
                                            &record,
                                            TaskOutcome::Orphaned {
                                                reason: reason.as_str().to_owned(),
                                            },
                                        )
                                        .await?;
                                }
                            }
                            Resolution::Ready {
                                kind,
                                record: resolved,
                            } => {
                                let checkpoint =
                                    state_checkpoint(&resolved).cloned().unwrap_or(Value::Null);
                                if resolved != record
                                    || record.state.status() != TaskStatus::Running
                                {
                                    sched
                                        .write_task(
                                            tx,
                                            with_state(
                                                &resolved,
                                                TaskState::Running { checkpoint },
                                            ),
                                        )
                                        .await?;
                                }
                                // 在线上登记，标记与后来的 reservation 才看得见它，close 才 join 得到。
                                let invocation = Invocation::new(&record, mode);
                                sched.st().invocations.insert(record.id, invocation.clone());
                                reservations.push(Reservation { invocation, kind });
                            }
                        }
                    }
                    Ok((reservations, abandoned))
                })
            })
            .await;
        match result {
            Ok((reservations, abandoned)) => {
                let marked = !abandoned.is_empty();
                {
                    let mut st = self.st();
                    for id in abandoned {
                        st.abandoned.remove(&id);
                    }
                    if marked {
                        st.dirty = true;
                    }
                }
                Ok(reservations)
            }
            Err(error) => {
                // 提交没落地：把闭包登记过的、还没起跑的 invocation 撤掉。
                let stale: Vec<Arc<Invocation>> = {
                    let mut st = self.st();
                    let ids: Vec<TaskId> = st
                        .invocations
                        .iter()
                        .filter(|(_, inv)| !inv.started.load(Ordering::SeqCst))
                        .map(|(id, _)| *id)
                        .collect();
                    ids.into_iter()
                        .filter_map(|id| st.invocations.remove(&id))
                        .collect()
                };
                for invocation in stale {
                    invocation.ended.store(true, Ordering::SeqCst);
                    invocation.token.cancel();
                    invocation.finish();
                }
                Err(error)
            }
        }
    }

    fn start(self: &Arc<Self>, reservation: Reservation) {
        let sched = self.clone();
        reservation.invocation.started.store(true, Ordering::SeqCst);
        tokio::spawn(async move {
            let invocation = reservation.invocation.clone();
            let outcome = AssertUnwindSafe(async {
                match invocation.mode {
                    InvocationMode::Run => sched.run(&reservation).await,
                    InvocationMode::Abort => sched.run_abort(&reservation).await,
                }
            })
            .catch_unwind()
            .await;
            if let Err(panic) = outcome {
                sched.report_text(&format!(
                    "task {} invocation panicked: {}",
                    invocation.task,
                    panic_message(panic)
                ));
            }
            sched.end(&invocation);
            invocation.finish();
            sched.kick();
        });
    }

    /// 结束一个 invocation：它的 runtime 操作此后都拒绝，信号取消，任务重新可被 reserve。
    pub fn end(&self, invocation: &Arc<Invocation>) {
        if invocation.ended.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut st = self.st();
            if st
                .invocations
                .get(&invocation.task)
                .is_some_and(|current| Arc::ptr_eq(current, invocation))
            {
                st.invocations.remove(&invocation.task);
            }
        }
        invocation.token.cancel();
    }

    fn runtime(self: &Arc<Self>, invocation: &Arc<Invocation>) -> TaskRuntime {
        TaskRuntime::new(self.clone(), invocation.clone())
    }

    // ------------------------------------------------------------------ run

    async fn run(self: &Arc<Self>, reservation: &Reservation) -> () {
        let invocation = &reservation.invocation;
        let kind = reservation.kind.clone();
        let mut reported: Option<usize> = None;
        let runtime = self.runtime(invocation);
        let mut previous: Option<PhaseReturn> = None;
        loop {
            // 换定义的判定在线外取快照；落写与停止仍在线上的 step 里。
            let snapshot = self.registry.snapshot();
            let replacement = match (&previous, self.st().graph.live_record(invocation.task)) {
                (Some(_), Some(record)) => replacement_for(&kind, &snapshot, record),
                _ => Replacement::Unchanged,
            };
            let (decision, current) = self.step(invocation, previous.clone(), replacement).await;
            if let StepDecision::Continue { report_unavailable } = decision
                && previous.is_some()
                && report_unavailable
            {
                let next = snapshot.task(
                    &self
                        .st()
                        .graph
                        .live_record(invocation.task)
                        .map(|r| r.kind.clone())
                        .unwrap_or_default(),
                );
                let id = next.as_ref().map_or(0, identity);
                if reported != Some(id) {
                    reported = Some(id);
                    self.report_text(&format!(
                        "Task {} keeps running under its old definition ({})",
                        invocation.task,
                        if next.is_none() {
                            "missing_task"
                        } else {
                            "incompatible_task"
                        }
                    ));
                }
            }
            let Some(current) = current else { return };
            if self.st().closing {
                return;
            }
            let checkpoint = current.checkpoint.clone();
            let outcome = AssertUnwindSafe(kind.run_phase(&current, &runtime))
                .catch_unwind()
                .await;
            let failure = match outcome {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(panic) => Some(panic_message(panic)),
            };
            previous = Some(PhaseReturn {
                checkpoint,
                failure,
            });
        }
    }

    async fn run_abort(self: &Arc<Self>, reservation: &Reservation) -> () {
        let invocation = &reservation.invocation;
        let current = self
            .st()
            .graph
            .live_record(invocation.task)
            .and_then(RunningTask::from_record);
        let Some(current) = current else { return };
        if self.st().closing {
            return;
        }
        let runtime = self.runtime(invocation);
        let outcome = AssertUnwindSafe(reservation.kind.abort(&current, &runtime))
            .catch_unwind()
            .await;
        let failure = match outcome {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(panic) => Some(panic_message(panic)),
        };
        let previous = PhaseReturn {
            checkpoint: current.checkpoint,
            failure,
        };
        self.step(invocation, Some(previous), Replacement::Unchanged)
            .await;
    }

    /// 线上的一次同步决定（spec §5.1 的 step）。invocation 在决定它结束的这个闭包里结束，
    /// 先于故障清理。被拒的 step（比如关闭后）同样结束它。
    async fn step(
        self: &Arc<Self>,
        invocation: &Arc<Invocation>,
        previous: Option<PhaseReturn>,
        replacement: Replacement,
    ) -> (StepDecision, Option<RunningTask>) {
        let sched = self.clone();
        let inv = invocation.clone();
        let result = self
            .line
            .commit(None, move |tx| {
                Box::pin(async move {
                    let (record, closing, owner_cancelled) = {
                        let st = sched.st();
                        let record = st.graph.live_record(inv.task).cloned();
                        let owner_cancelled = record.as_ref().is_some_and(|r| {
                            !r.background
                                && st
                                    .graph
                                    .cancelling_owner(TaskNode::of(r).parent())
                                    .is_some()
                        });
                        (record, st.closing, owner_cancelled)
                    };
                    let decision = decide_step(&StepInput {
                        mode: inv.mode,
                        task: record.as_ref(),
                        closing,
                        owner_cancelled,
                        previous: previous.as_ref(),
                        replacement,
                    });
                    match (&decision, record.as_ref()) {
                        (StepDecision::Continue { .. }, _) => {}
                        (StepDecision::Stop, _) => sched.end(&inv),
                        (StepDecision::StopAndCascade, _) => {
                            sched.end(&inv);
                            sched.schedule_reconcile();
                        }
                        (StepDecision::Handover, Some(record)) => {
                            sched.end(&inv);
                            let checkpoint =
                                state_checkpoint(record).cloned().unwrap_or(Value::Null);
                            sched
                                .write_task(
                                    tx,
                                    with_state(record, TaskState::Pending { checkpoint }),
                                )
                                .await?;
                        }
                        (StepDecision::Fault { message }, Some(record)) => {
                            sched.end(&inv);
                            sched
                                .terminate(
                                    tx,
                                    record,
                                    TaskOutcome::Faulted {
                                        error: TaskOutcomeError {
                                            message: message.clone(),
                                            detail: None,
                                        },
                                    },
                                )
                                .await?;
                        }
                        _ => sched.end(&inv),
                    }
                    let running = if matches!(decision, StepDecision::Continue { .. }) {
                        record.as_ref().and_then(RunningTask::from_record)
                    } else {
                        None
                    };
                    Ok((decision, running))
                })
            })
            .await;
        match result {
            Ok(done) => done,
            Err(error) => {
                // step 写的是调度器自己的决定；写不下去就结束，免得任务停在 running 又被再跑。
                self.end(invocation);
                self.fail_session(error);
                (StepDecision::Stop, None)
            }
        }
    }

    /// 写调度器自己决定的结局（`faulted` / `orphaned`）。名下有活的普通工作时先 `completing`
    /// 握着，Harness 清理推迟到最终提交（spec §5.5 规则 4）；否则直接 terminal 并清理。
    async fn terminate(
        &self,
        tx: &mut Tx,
        record: &TaskRecord,
        outcome: TaskOutcome,
    ) -> Result<(), DurableError> {
        self.load_scopes().await?;
        let overlay = tx.overlay();
        let held = self
            .st()
            .graph
            .has_owned_live(record.id, Some(&overlay), &HashSet::new());
        if held {
            return self
                .write_task(tx, with_state(record, TaskState::Completing { outcome }))
                .await;
        }
        self.write_task(
            tx,
            with_state(
                record,
                TaskState::Terminal {
                    outcome: outcome.clone(),
                },
            ),
        )
        .await?;
        self.hooks.settle_outcome(tx, record, &outcome).await
    }

    // ------------------------------------------------------------------ runtime 提交

    /// runtime 的提交：线上重读任务并把关（invocation 已结束、Session 关闭、任务不是 running、
    /// run 模式下已有 abort 标记，都拒绝）。
    pub async fn gated<T: Send>(
        self: &Arc<Self>,
        invocation: &Arc<Invocation>,
        change: impl for<'a> FnOnce(&'a mut Tx, &'a TaskRecord) -> TxFuture<'a, T> + Send + 'static,
    ) -> Result<T, DurableError> {
        invocation.check()?;
        let sched = self.clone();
        let inv = invocation.clone();
        self.line
            .commit(
                Some(TaskScope {
                    conversation: invocation.conversation,
                    task: invocation.task,
                }),
                move |tx| {
                    Box::pin(async move {
                        inv.check()?;
                        if sched.st().closing {
                            return Err(sched.line.closed_error());
                        }
                        let found = sched.st().graph.live_record(inv.task).cloned();
                        let Some(current) = found else {
                            return Err(DurableError::rejected(format!(
                                "Task {} is terminal",
                                inv.task
                            )));
                        };
                        if current.state.status() != TaskStatus::Running {
                            return Err(DurableError::rejected(format!(
                                "Task {} is {:?}",
                                inv.task,
                                current.state.status()
                            )));
                        }
                        if inv.mode == InvocationMode::Run && current.abort_requested {
                            return Err(DurableError::rejected(format!(
                                "Task {} has a durable abort mark",
                                inv.task
                            )));
                        }
                        change(tx, &current).await
                    })
                },
            )
            .await
    }

    /// 把任务提交的下一个状态落成记录：等待先校验；结局在名下有活的普通工作（按本次候选
    /// 判断）时先 `completing`。
    pub async fn commit_state(
        &self,
        tx: &mut Tx,
        invocation: &Arc<Invocation>,
        current: &TaskRecord,
        next: NextTaskState,
    ) -> Result<(), DurableError> {
        check_next_state(&next)?;
        if let NextTaskState::Waiting { on, policy, .. } = &next {
            self.validate_wait(tx, invocation, current, on, *policy)
                .await?;
        }
        if let NextTaskState::Terminal { outcome } = &next {
            self.load_scopes().await?;
            let overlay = tx.overlay();
            for record in overlay.tasks.values() {
                self.load_chain(TaskNode::of(record).parent(), Some(&overlay))
                    .await?;
            }
            let held = self
                .st()
                .graph
                .has_owned_live(current.id, Some(&overlay), &HashSet::new());
            if held {
                return self
                    .write_task(
                        tx,
                        with_state(
                            current,
                            TaskState::Completing {
                                outcome: outcome.clone(),
                            },
                        ),
                    )
                    .await;
            }
        }
        self.write_task(tx, with_state(current, next.into_state()))
            .await
    }

    /// 等待要点已存在的任务；`failFast` 只能等自己拥有的；不许成环；abort handler 不能等。
    async fn validate_wait(
        &self,
        tx: &mut Tx,
        invocation: &Arc<Invocation>,
        current: &TaskRecord,
        on: &[TaskId],
        policy: JoinPolicy,
    ) -> Result<(), DurableError> {
        if invocation.mode == InvocationMode::Abort {
            return Err(DurableError::rejected(format!(
                "Abort handler of task {} cannot wait",
                current.id
            )));
        }
        let overlay = tx.overlay();
        let mut targets = Vec::new();
        for id in on {
            let staged = overlay.tasks.get(id).cloned();
            let live = self.st().graph.live_record(*id).cloned();
            let member = match staged.or(live) {
                Some(record) => Some(record),
                None => self.storage.task(*id).await?,
            };
            let Some(member) = member else {
                return Err(DurableError::rejected(format!("Task {id} does not exist")));
            };
            if policy == JoinPolicy::FailFast && member.owner != Some(current.id) {
                return Err(DurableError::rejected(format!(
                    "Task {} can wait failFast only on tasks it owns; {id} is not one",
                    current.id
                )));
            }
            // 活的子任务本来就拖着等待者，没有新边；已终结的没有边。
            if is_live(&member) && member.owner != Some(current.id) {
                targets.push(*id);
            }
        }
        if targets.is_empty() {
            return Ok(());
        }
        self.load_scopes().await?;
        let cycle = self.st().graph.wait_cycle(current.id, &targets, &overlay);
        if let Some(cycle) = cycle {
            let path: Vec<String> = std::iter::once(current.id)
                .chain(cycle.iter().copied())
                .map(|id| id.to_string())
                .collect();
            return Err(DurableError::rejected(format!(
                "Task {} cannot wait on {}: {} is a wait cycle",
                current.id,
                cycle[0],
                path.join(" -> ")
            )));
        }
        Ok(())
    }

    // ------------------------------------------------------------------ reconcile

    fn schedule_reconcile(self: &Arc<Self>) {
        {
            let mut st = self.st();
            if st.reconcile_scheduled || st.closing {
                return;
            }
            st.reconcile_scheduled = true;
        }
        let sched = self.clone();
        tokio::spawn(async move { sched.reconcile().await });
    }

    /// 一笔提交把已提交记录隐含的事都落下：取消意图往下打标记、failFast 标记、没有活工作的
    /// `completing` 写最终 terminal。记录就是意图，所以这也修补崩溃留下的半截。
    async fn reconcile(self: Arc<Self>) {
        self.st().reconcile_scheduled = false;
        let sched = self.clone();
        let result = self
            .line
            .commit(None, move |tx| {
                Box::pin(async move {
                    if sched.st().closing {
                        return Ok(());
                    }
                    sched.load_scopes().await?;
                    let failed = sched.collect_failed().await?;
                    let marks = abort_marks(&sched.st().graph, &failed);
                    for mark in marks {
                        let record = sched.st().graph.live_record(mark.id).cloned();
                        if let Some(record) = record {
                            sched
                                .write_task(tx, with_abort_mark(&record, mark.reason))
                                .await?;
                        }
                    }
                    sched.finalize(tx).await
                })
            })
            .await;
        if let Err(error) = result {
            self.fail_session(error);
        }
        self.settle_idle();
    }

    /// failFast 等待者的 `on` 里已经握着或终结于非 `completed` 结局的任务。
    async fn collect_failed(&self) -> Result<HashSet<TaskId>, DurableError> {
        let members: Vec<TaskId> = {
            let st = self.st();
            st.graph
                .live()
                .filter_map(|record| match &record.state {
                    TaskState::Waiting {
                        on,
                        policy: JoinPolicy::FailFast,
                        ..
                    } => Some(on.clone()),
                    _ => None,
                })
                .flatten()
                .collect()
        };
        let mut failed = HashSet::new();
        for id in members {
            let live = self.st().graph.live_record(id).map(failed_outcome);
            let is_failed = match live {
                Some(is_failed) => is_failed,
                None => self
                    .storage
                    .task(id)
                    .await?
                    .is_some_and(|record| failed_outcome(&record)),
            };
            if is_failed {
                failed.insert(id);
            }
        }
        Ok(failed)
    }

    /// 给每个名下没有活的普通工作的 `completing` 写最终 terminal。收尾一个可能放开它上面的
    /// 所有者，所以重复到没有变化为止；调度器自己写的结局在这里做 Harness 清理。
    async fn finalize(&self, tx: &mut Tx) -> Result<(), DurableError> {
        let mut done: HashSet<TaskId> = HashSet::new();
        loop {
            let overlay = tx.overlay();
            let pick = {
                let st = self.st();
                let mut candidates: Vec<&TaskRecord> = st.graph.live().collect();
                candidates.extend(
                    overlay
                        .tasks
                        .values()
                        .filter(|record| !st.graph.is_live(record.id)),
                );
                let mut picked = None;
                for live in candidates {
                    let record = overlay.tasks.get(&live.id).unwrap_or(live);
                    if finalizable(&st.graph, record, Some(&overlay), &done) {
                        picked = Some(record.clone());
                        break;
                    }
                }
                picked
            };
            let Some(record) = pick else { return Ok(()) };
            let TaskState::Completing { outcome } = &record.state else {
                return Ok(());
            };
            let outcome = outcome.clone();
            done.insert(record.id);
            self.write_task(
                tx,
                with_state(
                    &record,
                    TaskState::Terminal {
                        outcome: outcome.clone(),
                    },
                ),
            )
            .await?;
            // 只有调度器写 `faulted` 与 `orphaned`，它们的清理等到这笔提交。
            if matches!(
                outcome,
                TaskOutcome::Faulted { .. } | TaskOutcome::Orphaned { .. }
            ) {
                self.hooks.settle_outcome(tx, &record, &outcome).await?;
            }
        }
    }

    // ------------------------------------------------------------------ 对外操作

    /// 提交 abort 标记；没有定义能接手的任务（且名下没有活工作）直接 `orphaned`。然后 join
    /// 线上看到的 run invocation。abort invocation 在名下的普通工作结束后才起。
    /// `keep_restart`：所有者自己的清理，不替换 restart 标记。
    pub async fn abort(
        self: &Arc<Self>,
        id: TaskId,
        keep_restart: bool,
    ) -> Result<AbortOutcome, DurableError> {
        let sched = self.clone();
        let (result, run) = self
            .line
            .commit(None, move |tx| {
                Box::pin(async move {
                    let current = tx.task(id).await?.ok_or(DurableError::NoSuchTask(id))?;
                    if matches!(current.state, TaskState::Terminal { .. }) {
                        return Ok((AbortOutcome::Terminal, None));
                    }
                    let invocation = sched.st().invocations.get(&id).cloned();
                    // 重启放弃的任务没有 run invocation，只等它的 abort。
                    if keep_restart && current.abort_reason == Some(AbortReason::Restart) {
                        return Ok((AbortOutcome::Marked, None));
                    }
                    if invocation.is_none() && current.state.status() != TaskStatus::Completing {
                        sched.load_scopes().await?;
                        let owned_live = sched.st().graph.has_owned_live(id, None, &HashSet::new());
                        if !owned_live
                            && let Resolution::Blocked(reason) =
                                sched.resolve(&current, &sched.registry.snapshot())
                        {
                            sched
                                .terminate(
                                    tx,
                                    &current,
                                    TaskOutcome::Orphaned {
                                        reason: reason.as_str().to_owned(),
                                    },
                                )
                                .await?;
                            return Ok((AbortOutcome::Marked, None));
                        }
                    }
                    if !current.abort_requested || current.abort_reason.is_some() {
                        sched
                            .write_task(tx, with_abort_mark(&current, None))
                            .await?;
                    }
                    let run = invocation.filter(|inv| inv.mode == InvocationMode::Run);
                    Ok((AbortOutcome::Marked, run))
                })
            })
            .await?;
        // 提交监听已给 run 发了信号；join 它。不听信号的 run 可能比失败的 Session 活得久，
        // Session 封口就结束这次等待。
        if let Some(run) = run {
            tokio::select! {
                _ = run.wait_done() => {}
                _ = self.line.sealed() => {}
            }
        }
        Ok(result)
    }

    pub async fn wait_for_task(self: &Arc<Self>, id: TaskId) -> Result<TaskRecord, DurableError> {
        #[allow(clippy::large_enum_variant)]
        enum Found {
            Done(TaskRecord),
            Pending(oneshot::Receiver<Result<TaskRecord, DurableError>>),
        }
        let sched = self.clone();
        // 在线上检查并登记，终结发布不会落在两者之间。
        let found = self
            .line
            .read_on_line(move || {
                Box::pin(async move {
                    if sched.st().closing {
                        return Err(sched.line.closed_error());
                    }
                    let live = sched.st().graph.is_live(id);
                    if live {
                        let (tx, rx) = oneshot::channel();
                        sched.st().task_waiters.entry(id).or_default().push(tx);
                        return Ok(Found::Pending(rx));
                    }
                    match sched.storage.task(id).await? {
                        Some(record) => Ok(Found::Done(record)),
                        None => Err(DurableError::NoSuchTask(id)),
                    }
                })
            })
            .await?;
        match found {
            Found::Done(record) => Ok(record),
            Found::Pending(rx) => rx.await.map_err(|_| self.line.closed_error())?,
        }
    }

    pub async fn wait_for_idle(
        self: &Arc<Self>,
        conversation: Option<ConversationId>,
    ) -> Result<(), DurableError> {
        let rx = {
            let mut st = self.st();
            if st.closing {
                drop(st);
                return Err(self.line.closed_error());
            }
            if Self::idle_in(&st, conversation) {
                return Ok(());
            }
            let (tx, rx) = oneshot::channel();
            st.idle_waiters.push(IdleWaiter { conversation, tx });
            rx
        };
        self.schedule_reconcile();
        rx.await.map_err(|_| self.line.closed_error())?
    }

    /// `Conversation.abort()`：一笔提交里给普通遍历从该 conversation 够到的活的非后台任务
    /// 打标记，然后等范围空闲；`background` 时遍历穿过后台边界，并等每个被标记的任务终结。
    pub async fn abort_conversation(
        self: &Arc<Self>,
        conversation: ConversationId,
        background: bool,
    ) -> Result<(), DurableError> {
        let sched = self.clone();
        let reached = self
            .line
            .commit(None, move |tx| {
                Box::pin(async move {
                    sched.load_scopes().await?;
                    sched
                        .load_chain(Node::Conversation(conversation), None)
                        .await?;
                    let walked = sched.st().graph.walk_down(
                        Node::Conversation(conversation),
                        None,
                        background,
                        &|_| true,
                    );
                    let mut reached = Vec::new();
                    for item in walked {
                        let Some(live) = item.live else { continue };
                        if item.node.background && !background {
                            continue;
                        }
                        reached.push(live.id);
                        if !live.abort_requested || live.abort_reason.is_some() {
                            sched.write_task(tx, with_abort_mark(&live, None)).await?;
                        }
                    }
                    Ok(reached)
                })
            })
            .await?;
        if background {
            for id in reached {
                self.wait_for_task(id).await?;
            }
        }
        self.wait_for_idle(Some(conversation)).await
    }

    pub fn inspect_now(&self) -> Vec<TaskInspection> {
        let snapshot = self.registry.snapshot();
        let st = self.st();
        st.graph
            .live()
            .map(|record| {
                let state = if st.invocations.contains_key(&record.id) {
                    TaskInspectionState::Running
                } else if record.state.status() == TaskStatus::Completing {
                    TaskInspectionState::Completing
                } else {
                    let on = waiting_on(&st.graph, record, false);
                    if !on.is_empty() {
                        TaskInspectionState::Waiting { on }
                    } else {
                        let kind = snapshot.task(&record.kind);
                        let failed = kind.as_ref().is_some_and(|k| {
                            st.failed_migrations.get(&record.id) == Some(&identity(k))
                        });
                        match fit(record.version, kind.map(|k| k.info()), failed) {
                            Fit::Blocked(reason) => TaskInspectionState::Blocked { reason },
                            Fit::Ready { migrates } => TaskInspectionState::Ready { migrates },
                        }
                    }
                };
                TaskInspection {
                    record: record.clone(),
                    state,
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortOutcome {
    /// 标记已提交（或任务正在收尾）；等 `wait_for_task` 看结局。
    Marked,
    /// 任务早已 terminal。
    Terminal,
}

fn replacement_for(
    current: &Arc<dyn TaskKind>,
    snapshot: &RegistrySnapshot,
    record: &TaskRecord,
) -> Replacement {
    match snapshot.task(&record.kind) {
        Some(next) if identity(&next) == identity(current) => Replacement::Unchanged,
        Some(next) if can_reserve(record.version, next.info()) => Replacement::Takeover,
        _ => Replacement::Unavailable,
    }
}
