//! 调度器领到一个 Run 之后交给谁（`scheduler::RunHandler` 的生产实现）。
//!
//! 职责边界抄自 `RunHandler` 的文档：**终态与挂起都由实现自己写进账本**，调度器不替
//! 它判断跑成什么样。这里就是那句话的落地——[`AgentLoop::run`] 返回时账本上已经有了
//! 终态或一个 [`WaitReason`](komo_kernel::types::status::WaitReason)，所以 handler
//! 返回 `Ok(())`；只有**账本本身写不进去**才是 `Err`，那时调度器只 `release`，什么都
//! 没写下，下一轮重新领取。
//!
//! 另外半个职责是**租约心跳**（§8.7）：一个二十分钟的调用不该因为"十分钟没人写账本"
//! 就被当成没人管。心跳由 [`AgentRunHandler::heartbeat`] 起，和这一段同生共死。
//!
//! 装配一个执行段要三样这里看不见的东西：Session 的工作目录与工作区根（在 store
//! 里）、`TurnRequest`（系统提示、回放窗口、记忆注入，在 `llm` / `memory` 侧）、恢复
//! 位置（`recovery` 折出来的）。所以它们从一条缝进来：[`SegmentSource`]。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use komo_kernel::traits::{Ledger, LedgerError, RunQueue, StoreError};
use komo_kernel::types::ids::{ExecutorId, InterventionId};
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::status::{Claimed, WaitReason};
use komo_kernel::types::tool::{CancelToken, ToolDefinition};
use tracing::Instrument;

use crate::executor::{ExecError, ToolExecutor};
use crate::scheduler::{HandlerError, RunHandler};

use super::{AgentError, AgentLoop, Segment, SegmentOutcome};

/// 续租间隔（§8.7）。**每 30 秒把租约推到 `now + 90s`**（[`LEASE_AHEAD`]）。
///
/// 窗口由队列句柄带（`TursoRunQueue::with_lease(db, 2min)` 在领取时写下
/// `lease_until = claimed_at + 2min`），kernel 的 `claim` 没有 TTL 参数，所以这里定的
/// 只是**续租的节拍与往前推多远**，不是租约窗口本身。
///
/// 为什么必须显著长于间隔（这里是 3 倍）：一次续租失败、一次 GC 停顿或磁盘抖动，都可能
/// 把两拍挤在一起；间隔太长会让一个**活着的** run 看起来过期。而"看起来过期"只是**信号**
/// 不是结论——对账回收前还要过一道"持有者确已不在"（`claimed_by <> self`，或自己内存里
/// 已不再持有它）。少了那道判定，一次二十分钟的调用会在主人还活着的时候被第二个执行者
/// 抢走：那不是恢复，是重复副作用（§8.6）。
const RENEW_INTERVAL: Duration = Duration::from_secs(30);

/// 每一次续租把租约推到多久之后（见 [`RENEW_INTERVAL`] 的 3 倍关系）。
const LEASE_AHEAD: time::Duration = time::Duration::seconds(90);

/// 一次领取里最多压几次再重新装配（[`SegmentOutcome::Recompose`]）。到了就不再压，
/// 照常跑完这一段——压缩是省钱的手段，不该变成一个不前进的环。
const MAX_RECOMPOSITIONS: u32 = 8;

/// 后台续租的那个任务。**它和这一段同生共死**：`Drop` 即 `abort`。
///
/// 为什么不能让它留着（哪怕一次）：一个**已经结束**的 Run 继续续租，会让对账永远看不到
/// 它是孤儿——租约一直是新的，"这个人不在了"这个信号就永远不发出来，而那条 Run 会一直
/// 停在 `running` 上白占一个并发名额（§8.7、§8.9）。所以它不是"跑着跑着自己退出的后台
/// 任务"，而是**绑在这一段生命周期上的守卫**。
///
/// `abort` 撞在一次续租中途也不要紧：续租是一句带代次围栏的 `UPDATE`，要么提交要么没有，
/// 不会留下半条写入；最坏是这一拍没续上，而那时这一段已经结束了。
struct Heartbeat {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Run 号 → 这一段要的全部上下文。
///
/// 一个 `async` 闭包也能实现它（见 [`from_fn`]），所以上层不必为此造一个类型。
#[async_trait]
pub trait SegmentSource: Send + Sync {
    /// `tools` 是执行器当前挂载的工具 Schema——装配 `TurnRequest` 要它，而"挂了哪些
    /// 工具"是执行器的事实，不该由装配方再抄一份。
    async fn segment(
        &self,
        claimed: &Claimed,
        tools: Vec<ToolDefinition>,
    ) -> Result<Segment, HandlerError>;
}

/// 用一个闭包当 [`SegmentSource`]。
pub fn from_fn<F, Fut>(f: F) -> impl SegmentSource
where
    F: Fn(Claimed, Vec<ToolDefinition>) -> Fut + Send + Sync,
    Fut: Future<Output = Result<Segment, HandlerError>> + Send,
{
    struct FromFn<F>(F);

    #[async_trait]
    impl<F, Fut> SegmentSource for FromFn<F>
    where
        F: Fn(Claimed, Vec<ToolDefinition>) -> Fut + Send + Sync,
        Fut: Future<Output = Result<Segment, HandlerError>> + Send,
    {
        async fn segment(
            &self,
            claimed: &Claimed,
            tools: Vec<ToolDefinition>,
        ) -> Result<Segment, HandlerError> {
            (self.0)(claimed.clone(), tools).await
        }
    }

    FromFn(f)
}

/// [`RunHandler`] 的生产实现。
pub struct AgentRunHandler {
    agent: Arc<AgentLoop>,
    executor: Arc<ToolExecutor>,
    ledger: Arc<dyn Ledger>,
    source: Arc<dyn SegmentSource>,
    /// 续租的那条路（§8.7）。`claim` 由调度器走，这里只用来在跑着的时候续租。
    queue: Arc<dyn RunQueue>,
    executor_id: ExecutorId,
}

impl std::fmt::Debug for AgentRunHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRunHandler")
            .field("executor", &self.executor_id)
            .finish_non_exhaustive()
    }
}

impl AgentRunHandler {
    pub fn new(
        agent: Arc<AgentLoop>,
        executor: Arc<ToolExecutor>,
        ledger: Arc<dyn Ledger>,
        source: Arc<dyn SegmentSource>,
        queue: Arc<dyn RunQueue>,
        executor_id: ExecutorId,
    ) -> Self {
        Self {
            agent,
            executor,
            ledger,
            source,
            queue,
            executor_id,
        }
    }

    /// 起一个后台续租：每 [`RENEW_INTERVAL`] 把租约推到 `now + `[`LEASE_AHEAD`]（§8.7）。
    ///
    /// 返回的 [`Heartbeat`] 一 drop 就停——调用方把它绑在 `attempt` 的局部变量上即可。
    fn heartbeat(&self, claimed: &Claimed, cancel: CancelToken) -> Heartbeat {
        let queue = Arc::clone(&self.queue);
        let executor = self.executor_id.clone();
        let claimed = claimed.clone();
        let task = tokio::spawn(async move {
            let mut ticks = tokio::time::interval(RENEW_INTERVAL);
            // tick 挤在一起时**往后顺延**，不补跑：这里要的是"最近一次续租到现在有多
            // 久"，补跑一串已经过时的续租没有任何意义。
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // `interval` 的第一次 tick 立刻到。领取那一句已经写下租约了
            // （`lease_until = claimed_at + 窗口`），所以这一次不必再续。
            ticks.tick().await;
            loop {
                ticks.tick().await;
                let until = time::OffsetDateTime::now_utc() + LEASE_AHEAD;
                match queue.renew(&claimed, &executor, until).await {
                    // **代次围栏的租约版**：这个 Run 已经不是自己的了。与状态提交拿到
                    // `StaleGeneration` 是同一件事，做法也一样——停止这个任务的一切写入。
                    Ok(false) => {
                        tracing::warn!(
                            run = %claimed.run,
                            generation = claimed.generation,
                            "领取权已不属于本实例，停止一切写入"
                        );
                        // 让这一段当场收尾（循环的每个 await 都和这个令牌赛跑，见
                        // `executor::cancel::race`），于是它走 `RunEnd::Cancelled` 而不是
                        // 接着往账本上写。取消之后本任务就退出；真正把它按下的是 `Drop`。
                        cancel.cancel();
                        return;
                    }
                    Ok(true) => tracing::debug!(run = %claimed.run, "租约已续"),
                    // 一次续租写不进去不等于租约丢了：下一拍再试。真丢了也不会有人凭
                    // 一次过期就抢活——对账前面还有一道存活判定（§8.7）。
                    Err(error) => {
                        tracing::warn!(run = %claimed.run, %error, "续租失败，下一拍再试");
                    }
                }
            }
        });
        Heartbeat { task }
    }

    /// §7.5：操作者对一次「结果不明」的调用下的结论，落到那次尝试的账上。
    ///
    /// 它**不碰 Run 的状态**：写完这条结果，调用方（网关）按 §8.4 把 Run 重新入队或
    /// 取消——"谁来接手"是调度的事，"这一次调用发生了什么"才是这里的事。
    pub async fn settle_by_operator(
        &self,
        session: &komo_kernel::types::ids::SessionId,
        run: &komo_kernel::types::ids::RunId,
        call: &komo_kernel::types::ids::ToolCallId,
        attempt: &komo_kernel::types::ids::AttemptId,
        verdict: crate::executor::OperatorVerdict,
    ) -> Result<komo_kernel::types::refs::ToolResultStatus, ExecError> {
        self.executor
            .settle_by_operator(session, run, call, attempt, verdict)
            .await
    }
}

#[async_trait]
impl RunHandler for AgentRunHandler {
    async fn run(&self, claimed: Claimed) -> Result<(), HandlerError> {
        // 一次领取 = 一个 Run 段 = 一个 `invoke_agent` span（`docs/observability.md` §3.1）。
        // Run 可以挂起、跨重启恢复，span 活不过进程，所以用 `komo.run.id` 把几段串起来。
        let span = tracing::info_span!(
            "invoke_agent",
            gen_ai.operation.name = "invoke_agent",
            gen_ai.agent.name = tracing::field::Empty,
            gen_ai.conversation.id = tracing::field::Empty,
            komo.run.id = %claimed.run,
            komo.run.source = tracing::field::Empty,
            komo.session.kind = tracing::field::Empty,
            komo.run.outcome = tracing::field::Empty,
        );
        let attempted = self.attempt(&claimed).instrument(span.clone()).await;
        if attempted.is_err() {
            span.record("komo.run.outcome", "error");
        }
        match attempted {
            Ok(()) => Ok(()),
            // 「JSONL 已提交范围缺失或中间损坏时停止受影响会话的自动执行」（§8.3、
            // §8.5）。停止的做法有两半，缺一不可：**账本上说清楚**（`waiting +
            // intervention`，操作者看得见"它停着、而且是在等人"），以及**不放回队列**
            // ——一个读不出来的会话下一次照样读不出来，交还领取权只会变成"领取 → 装配
            // 失败 → 交还"的空转。
            //
            // 理由（哪一段读不出来）在 `WaitReason` 里没有位置：它是句柄不是自由文本，
            // 所以这里留一条 error 日志，并把同一句话交给调度器（`Stopped`）。
            Err(error) if is_corrupt(&error) => {
                let reason = format!("会话损坏，已停止自动执行：{error}");
                tracing::error!(run = %claimed.run, %reason, "会话损坏，停成 waiting + intervention");
                let wait = WaitReason::Intervention {
                    // 一个 Run 上最多停着一条要人判断的干预，所以句柄就是这个 Run（§7.5）。
                    intervention: InterventionId::for_run(&claimed.run),
                };
                if let Err(write) = self.ledger.suspend(&claimed.run, wait).await {
                    // 连这条都写不进去，那就只剩"不放回队列"这一半。**不要用这次写
                    // 失败盖掉损坏本身**——报告里要看到的是损坏。
                    tracing::error!(
                        run = %claimed.run,
                        error = %write,
                        "会话损坏，且这条 waiting + intervention 也写不进去"
                    );
                }
                Err(HandlerError::Stopped { reason })
            }
            Err(other) => Err(other),
        }
    }
}

/// 一个 Run 段的标签：记到 `invoke_agent` span 上，也是 Run 指标的维度（低基数，不含 id）。
struct RunLabels {
    source: &'static str,
    session_kind: &'static str,
}

impl RunLabels {
    fn of(segment: &Segment) -> Self {
        let span = tracing::Span::current();
        span.record("gen_ai.conversation.id", segment.session.to_string());
        if let Some(agent) = &segment.telemetry.agent {
            span.record("gen_ai.agent.name", agent.as_str());
        }
        let source = match &segment.env.source {
            PlanSource::Interactive { .. } => "interactive",
            PlanSource::Cron { .. } => "cron",
            PlanSource::Memory { .. } => "memory",
            PlanSource::Verification { .. } => "verification",
        };
        span.record("komo.run.source", source);
        let session_kind = segment.telemetry.session_kind.unwrap_or("unknown");
        span.record("komo.session.kind", session_kind);
        RunLabels {
            source,
            session_kind,
        }
    }

    /// 这一段收尾：outcome 记到 span 上，并发 Run 耗时与轮数指标。`earlier` 是同一次领取里
    /// 之前几段（压缩、插话后重新装配）已经跑过的轮数。
    fn finish(&self, outcome: &SegmentOutcome, earlier: u32, elapsed: Duration) {
        let (word, rounds) = match outcome {
            SegmentOutcome::Completed { rounds, .. } => ("completed", *rounds),
            SegmentOutcome::Suspended { rounds, .. } => ("suspended", *rounds),
            SegmentOutcome::Failed { rounds, .. } => ("failed", *rounds),
            SegmentOutcome::Cancelled { rounds } => ("cancelled", *rounds),
            SegmentOutcome::Recompose { rounds, .. }
            | SegmentOutcome::Interjected { rounds, .. } => ("continued", *rounds),
        };
        tracing::Span::current().record("komo.run.outcome", word);
        tracing::event!(
            target: "komo::metrics",
            tracing::Level::DEBUG,
            {
            histogram.komo.run.duration = elapsed.as_secs_f64(),
            komo.run.outcome = word,
            komo.run.source = self.source,
            komo.session.kind = self.session_kind,
            }
        );
        tracing::event!(
            target: "komo::metrics",
            tracing::Level::DEBUG,
            {
            histogram.komo.run.rounds = u64::from(earlier + rounds),
            komo.run.outcome = word,
            komo.run.source = self.source,
            komo.session.kind = self.session_kind,
            }
        );
    }
}

/// 损坏：读不出来的东西下一次照样读不出来。
fn is_corrupt(error: &HandlerError) -> bool {
    matches!(
        error,
        HandlerError::Ledger(LedgerError::Corrupt(_)) | HandlerError::Store(StoreError::Corrupt(_))
    )
}

impl AgentRunHandler {
    async fn attempt(&self, claimed: &Claimed) -> Result<(), HandlerError> {
        // `run.started`：领取这件事得有人写下来，而 `RunQueue::claim` 只改数据库里的
        // 行。fold 认它——状态变 `Running`，代次记在 `RunView` 上。
        self.ledger
            .start_run(&claimed.run, &self.executor_id, claimed.generation)
            .await?;

        // 压成了就按日志重新装配、同一个 Run 接着跑（§6）：不重新入队，也不再写一次
        // `run.started`。这一次领取里已经跑过的轮数与 token 从下一段的预算里扣掉——
        // 不然每压一次预算就满血复活一次。
        let mut recompositions = 0;
        let mut used_rounds = 0u32;
        let mut used_tokens = 0u64;
        let mut previous_cancel: Option<CancelToken> = None;
        let started = std::time::Instant::now();
        let mut labels: Option<RunLabels> = None;
        let outcome = loop {
            let mut segment = self
                .source
                .segment(claimed, self.executor.catalog())
                .await?;
            if labels.is_none() {
                labels = Some(RunLabels::of(&segment));
            }
            if let Some(limit) = segment.budget.max_rounds.as_mut() {
                *limit = limit.saturating_sub(used_rounds);
            }
            if let Some(limit) = segment.budget.max_tokens.as_mut() {
                *limit = limit.saturating_sub(used_tokens);
            }
            // 取消按在上一段的令牌上、而新一段已经换了令牌：传过来，不丢。
            if previous_cancel
                .as_ref()
                .is_some_and(CancelToken::is_cancelled)
            {
                segment.env.cancel.cancel();
            }
            if recompositions >= MAX_RECOMPOSITIONS
                && (segment.compaction.is_some() || segment.compactor.is_some())
            {
                tracing::warn!(
                    run = %claimed.run,
                    recompositions,
                    "这一次领取里压缩重装配已到上限，这一段不再压"
                );
                segment.compaction = None;
                segment.compactor = None;
            }
            let cancel = segment.env.cancel.clone();

            // 续租要能**取消这一段**：取消令牌在段里（`CallEnv`），先克隆一份（同一个
            // `AtomicBool`）再把它交给 loop。心跳在装配之后起——装配阶段还拿不到令牌，
            // 而且那个阶段不写账本、不产生副作用，最长的那一段（模型调用与工具执行）在它
            // 后面。
            let heartbeat = self.heartbeat(claimed, cancel.clone());

            let outcome = self.agent.run(segment).await;
            // **这一段一返回，心跳就必须停**（否则一个已结束的 Run 会一直续租，对账
            // 永远看不到它是孤儿）。`Drop` 会 abort，这里写明是为了让顺序看得见。
            drop(heartbeat);

            match outcome {
                Ok(SegmentOutcome::Recompose { rounds, usage }) => {
                    recompositions += 1;
                    used_rounds += rounds;
                    used_tokens += super::spent(&usage);
                    previous_cancel = Some(cancel);
                }
                // 插话（§6）：同样按日志重新装配、同一个 Run 接着跑；预算照扣，但不占压缩
                // 的重装配次数——它的次数由人打了几句话决定，不会自己转圈。
                Ok(SegmentOutcome::Interjected { rounds, usage }) => {
                    used_rounds += rounds;
                    used_tokens += super::spent(&usage);
                    previous_cancel = Some(cancel);
                }
                other => break other,
            }
        };

        if let (Some(labels), Ok(done)) = (&labels, &outcome) {
            labels.finish(done, used_rounds, started.elapsed());
        }

        match outcome {
            // 终态与挂起都已经在账本上了（`AgentLoop::run` 返回前一定写过一条）。
            Ok(SegmentOutcome::Completed { .. })
            | Ok(SegmentOutcome::Suspended { .. })
            | Ok(SegmentOutcome::Failed { .. })
            | Ok(SegmentOutcome::Cancelled { .. }) => Ok(()),
            Ok(SegmentOutcome::Recompose { .. }) | Ok(SegmentOutcome::Interjected { .. }) => {
                unreachable!("上面的循环接着跑，不会带着它出来")
            }
            // 只有账本 / 存储写不进去才到这里：什么都没写下，调度器 release 之后
            // 下一轮重新领取。**不在这里补写一个终态**——那等于把一次写入失败说成
            // 一次执行结果。
            Err(AgentError::Ledger(error)) | Err(AgentError::Exec(ExecError::Ledger(error))) => {
                Err(HandlerError::Ledger(error))
            }
            Err(AgentError::Exec(ExecError::Store(error))) => Err(HandlerError::Store(error)),
            Err(AgentError::Exec(ExecError::Repo(error))) => {
                Err(HandlerError::Failed(error.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Budget, tests_support::*};
    use crate::executor::harness::Harness;
    use komo_kernel::test_support::{MemRunQueue, ScriptedLlm};
    use komo_kernel::traits::{LedgerError, LlmClient, RunQueue, TurnDriver};
    use komo_kernel::types::ids::{RunId, SessionId};
    use komo_kernel::types::model::TokenUsage;
    use komo_kernel::types::status::RunState;
    use komo_kernel::types::turn::{LlmError, Round, RoundInput, TurnRequest};
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// 装配一段：`Harness` 给的环境 + 一段普通的 `TurnRequest`。
    fn source_for(harness: &Harness, session: &SessionId, run: &RunId) -> Arc<dyn SegmentSource> {
        let env = harness.env(session, run);
        let (session, run) = (session.clone(), run.clone());
        Arc::new(from_fn(move |_claimed, _tools| {
            let env = env.clone();
            let (session, run) = (session.clone(), run.clone());
            async move {
                Ok(Segment {
                    telemetry: Default::default(),
                    request: turn_request(&session, &run),
                    env,
                    budget: Budget::default(),
                    resume: None,
                    command: None,
                    compaction: None,
                    compactor: None,
                    session,
                    run,
                })
            }
        })) as Arc<dyn SegmentSource>
    }

    fn handler(
        harness: &Harness,
        agent: Arc<AgentLoop>,
        executor: Arc<ToolExecutor>,
        source: Arc<dyn SegmentSource>,
        queue: Arc<dyn RunQueue>,
    ) -> AgentRunHandler {
        AgentRunHandler::new(
            agent,
            executor,
            harness.ledger.clone(),
            source,
            queue,
            ExecutorId::from_raw("exec-1"),
        )
    }

    /// 一个记着**租约表**的队列替身（§8.7）。
    ///
    /// 领取、交还与代次围栏用真的 [`MemRunQueue`]——那些行为不是这两个心跳测试的对象；
    /// 换掉的只有 `renew`：它记下每一次续租把租约推到什么时刻，并且可以答"你已经不是
    /// 主人了"。
    #[derive(Debug)]
    struct LeaseFake {
        inner: MemRunQueue,
        renews: AtomicUsize,
        leases: Mutex<BTreeMap<RunId, time::OffsetDateTime>>,
        ours: AtomicBool,
    }

    impl LeaseFake {
        fn new() -> Arc<Self> {
            Arc::new(LeaseFake {
                inner: MemRunQueue::new(),
                renews: AtomicUsize::new(0),
                leases: Mutex::new(BTreeMap::new()),
                ours: AtomicBool::new(true),
            })
        }

        fn enqueue(&self, run: RunId) {
            self.inner.enqueue(run);
        }

        /// 下一次续租还认不认这个执行者。
        fn ours(&self, ours: bool) {
            self.ours.store(ours, Ordering::SeqCst);
        }

        fn renews(&self) -> usize {
            self.renews.load(Ordering::SeqCst)
        }

        fn lease_of(&self, run: &RunId) -> Option<time::OffsetDateTime> {
            self.leases.lock().unwrap().get(run).copied()
        }
    }

    #[async_trait]
    impl RunQueue for LeaseFake {
        async fn claim(&self, executor: &ExecutorId) -> Result<Option<Claimed>, StoreError> {
            self.inner.claim(executor).await
        }

        async fn claim_run(
            &self,
            run: &RunId,
            executor: &ExecutorId,
        ) -> Result<Option<Claimed>, StoreError> {
            self.inner.claim_run(run, executor).await
        }

        async fn release(&self, claimed: &Claimed) -> Result<(), StoreError> {
            self.inner.release(claimed).await
        }

        async fn renew(
            &self,
            claimed: &Claimed,
            _executor: &ExecutorId,
            until: time::OffsetDateTime,
        ) -> Result<bool, StoreError> {
            self.renews.fetch_add(1, Ordering::SeqCst);
            if !self.ours.load(Ordering::SeqCst) {
                return Ok(false);
            }
            self.leases
                .lock()
                .unwrap()
                .insert(claimed.run.clone(), until);
            Ok(true)
        }
    }

    /// 一个**在虚拟时间里慢慢想**的模型：心跳要真的跳起来，段就得活够久。
    ///
    /// `think` 是虚拟时间（测试跑在 `start_paused` 上），所以"想 45 秒"不花 45 秒。
    struct SlowLlm {
        think: Duration,
    }

    #[async_trait]
    impl LlmClient for SlowLlm {
        async fn begin_turn(&self, _req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
            Ok(Box::new(SlowDriver { think: self.think }))
        }
    }

    struct SlowDriver {
        think: Duration,
    }

    #[async_trait]
    impl TurnDriver for SlowDriver {
        async fn next(&mut self, _input: RoundInput) -> Result<Round, LlmError> {
            tokio::time::sleep(self.think).await;
            Ok(round(1, Some("好了。"), vec![]))
        }

        fn usage(&self) -> TokenUsage {
            Default::default()
        }
    }

    /// 领一个 Run 下来——代次与真队列一致。
    async fn claim_one(queue: &Arc<LeaseFake>, run: &RunId) -> Claimed {
        queue.enqueue(run.clone());
        queue
            .claim(&ExecutorId::from_raw("exec-1"))
            .await
            .unwrap()
            .expect("领得到")
    }

    #[tokio::test]
    async fn a_finished_run_leaves_its_terminal_event_on_the_ledger_and_returns_ok() {
        let harness = Harness::new();
        let executor = harness.executor(vec![]);
        let agent = Arc::new(AgentLoop::new(
            Arc::new(ScriptedLlm::once(vec![round(1, Some("好了。"), vec![])])),
            harness.ledger.clone(),
            executor.clone(),
            Arc::new(harness.clock.clone()),
        ));
        let (session, run) = harness.open_run().await;
        let handler = handler(
            &harness,
            agent,
            executor,
            source_for(&harness, &session, &run),
            LeaseFake::new(),
        );

        handler
            .run(Claimed {
                run: run.clone(),
                generation: 1,
            })
            .await
            .expect("跑完了");

        let surface = harness.ledger.surface();
        let view = surface.runs.get(&run).expect("有这个 Run");
        assert_eq!(view.status, RunState::Completed);
        assert_eq!(view.generation, Some(1), "领取代次记下来了");
    }

    #[tokio::test]
    async fn a_source_that_cannot_assemble_fails_the_handler_without_writing_a_terminal_state() {
        let harness = Harness::new();
        let executor = harness.executor(vec![]);
        let agent = Arc::new(AgentLoop::new(
            Arc::new(ScriptedLlm::once(vec![])),
            harness.ledger.clone(),
            executor.clone(),
            Arc::new(harness.clock.clone()),
        ));
        let (_session, run) = harness.open_run().await;

        let source = Arc::new(from_fn(|_claimed, _tools| async {
            Err(HandlerError::Ledger(LedgerError::NotFound {
                what: "session".into(),
            }))
        })) as Arc<dyn SegmentSource>;

        let handler = handler(&harness, agent, executor, source, LeaseFake::new());
        let error = handler
            .run(Claimed {
                run: run.clone(),
                generation: 1,
            })
            .await
            .unwrap_err();
        assert!(matches!(error, HandlerError::Ledger(_)), "{error:?}");

        let surface = harness.ledger.surface();
        assert_eq!(
            surface.runs.get(&run).expect("有这个 Run").status,
            RunState::Running,
            "装配失败不该被写成一个执行结果"
        );
    }

    /// 一个损坏的会话：handler 说它已经停了，账本是 `waiting + intervention`，
    /// 而调度器**不把它放回队列**——否则就是"领取 → 装配失败 → 交还"的空转。
    #[tokio::test]
    async fn a_corrupt_session_stops_the_run_and_never_returns_to_the_queue() {
        let harness = Harness::new();
        let executor = harness.executor(vec![]);
        let agent = Arc::new(AgentLoop::new(
            Arc::new(ScriptedLlm::once(vec![])),
            harness.ledger.clone(),
            executor.clone(),
            Arc::new(harness.clock.clone()),
        ));
        let (_session, run) = harness.open_run().await;

        let source = Arc::new(from_fn(|_claimed, _tools| async {
            Err(HandlerError::Ledger(LedgerError::Corrupt(
                "seq 43 与 45 之间缺了一行".into(),
            )))
        })) as Arc<dyn SegmentSource>;

        let queue = LeaseFake::new();
        let handler = handler(
            &harness,
            agent,
            executor,
            source,
            Arc::clone(&queue) as Arc<dyn RunQueue>,
        );

        // 调度器那一侧：领了它，handler 失败，按 `returns_to_queue()` 决定放不放回。
        let claimed = claim_one(&queue, &run).await;
        assert_eq!(queue.inner.depth(), 0);

        let error = handler.run(claimed.clone()).await.unwrap_err();
        let HandlerError::Stopped { reason } = &error else {
            panic!("{error:?}")
        };
        assert!(reason.contains("损坏"), "{reason}");
        assert!(!error.returns_to_queue(), "损坏的会话不该回到队列里");

        let view = harness
            .ledger
            .surface()
            .runs
            .get(&run)
            .cloned()
            .expect("有这个 Run");
        assert_eq!(
            view.status,
            RunState::Waiting,
            "停在等人上，不是一个说不清的停法"
        );
        assert_eq!(
            view.wait,
            Some(WaitReason::Intervention {
                intervention: InterventionId::for_run(&run)
            }),
            "「为什么不能跑」答得出来"
        );

        // 调度器照 `returns_to_queue()` 行事：不 release。队列仍然是空的。
        if error.returns_to_queue() {
            queue.release(&claimed).await.expect("交还名额");
        }
        assert_eq!(queue.inner.depth(), 0, "不放回去，才不会空转");
    }

    /// 存储侧的损坏走同一条路。
    #[tokio::test]
    async fn a_corrupt_output_reference_stops_the_run_too() {
        let harness = Harness::new();
        let executor = harness.executor(vec![]);
        let agent = Arc::new(AgentLoop::new(
            Arc::new(ScriptedLlm::once(vec![])),
            harness.ledger.clone(),
            executor.clone(),
            Arc::new(harness.clock.clone()),
        ));
        let (_session, run) = harness.open_run().await;

        let source = Arc::new(from_fn(|_claimed, _tools| async {
            Err(HandlerError::Store(
                komo_kernel::traits::StoreError::Corrupt("output.json 哈希不符".into()),
            ))
        })) as Arc<dyn SegmentSource>;

        let handler = handler(&harness, agent, executor, source, LeaseFake::new());
        let error = handler
            .run(Claimed {
                run: run.clone(),
                generation: 1,
            })
            .await
            .unwrap_err();
        assert!(matches!(error, HandlerError::Stopped { .. }), "{error:?}");
        assert!(!error.returns_to_queue());
        let view = harness.ledger.surface().runs.get(&run).cloned().unwrap();
        assert_eq!(view.status, RunState::Waiting);
        assert_eq!(
            view.wait,
            Some(WaitReason::Intervention {
                intervention: InterventionId::for_run(&run)
            })
        );
    }

    /// 普通失败（不是损坏）照旧交还领取权，让它能被再领一次。
    #[tokio::test]
    async fn an_ordinary_failure_still_returns_to_the_queue() {
        let harness = Harness::new();
        let executor = harness.executor(vec![]);
        let agent = Arc::new(AgentLoop::new(
            Arc::new(ScriptedLlm::once(vec![])),
            harness.ledger.clone(),
            executor.clone(),
            Arc::new(harness.clock.clone()),
        ));
        let (_session, run) = harness.open_run().await;

        let source = Arc::new(from_fn(|_claimed, _tools| async {
            Err(HandlerError::Ledger(LedgerError::Contended))
        })) as Arc<dyn SegmentSource>;

        let handler = handler(&harness, agent, executor, source, LeaseFake::new());
        let error = handler
            .run(Claimed {
                run: run.clone(),
                generation: 1,
            })
            .await
            .unwrap_err();
        assert!(
            error.returns_to_queue(),
            "写入争用下一次可能就过去了：{error:?}"
        );
        assert_eq!(
            harness.ledger.surface().runs.get(&run).unwrap().status,
            RunState::Running,
            "争用不是一个执行结果，也不是损坏"
        );
    }

    /// 续租被代次围栏拒了：**取消这个 Run**，不再有任何写入（§8.7）。
    ///
    /// 这是围栏的租约版——`renew` 答 `false` 与状态提交拿到 `StaleGeneration` 是同一件
    /// 事。可观察的形状：那个"想一小时"的段没有跑完，账本上留下 `cancelled`。
    #[tokio::test(start_paused = true)]
    async fn a_renewal_that_is_no_longer_ours_cancels_the_run() {
        let harness = Harness::new();
        let executor = harness.executor(vec![]);
        let agent = Arc::new(AgentLoop::new(
            Arc::new(SlowLlm {
                think: Duration::from_secs(3600),
            }),
            harness.ledger.clone(),
            executor.clone(),
            Arc::new(harness.clock.clone()),
        ));
        let (session, run) = harness.open_run().await;

        let queue = LeaseFake::new();
        queue.ours(false); // 第一次续租就答"你已经不是主人了"
        let claimed = claim_one(&queue, &run).await;

        let handler = handler(
            &harness,
            agent,
            executor,
            source_for(&harness, &session, &run),
            Arc::clone(&queue) as Arc<dyn RunQueue>,
        );
        // 虚拟时间里的兜底：心跳没取消的话，这一段会一直想到一小时。
        let outcome = tokio::time::timeout(Duration::from_secs(120), handler.run(claimed))
            .await
            .expect("续租被拒就该取消这一段，而不是让它接着跑");

        outcome.expect("取消是一个终态，不是一次写入失败");
        assert!(queue.renews() >= 1, "心跳确实跳过");
        let view = harness.ledger.surface().runs.get(&run).cloned().unwrap();
        assert_eq!(
            view.status,
            RunState::Cancelled,
            "领取权不再属于自己的那一刻就收尾"
        );
    }

    /// 心跳不能泄漏：**`attempt` 结束后一次续租都不该再发生**。
    ///
    /// 一个已结束的 Run 继续续租，会让对账永远看不到它是孤儿（§8.7、§8.9）——租约一直
    /// 是新的，"这个人不在了"这个信号就永远发不出来，那条 Run 会一直占着 `running`。
    #[tokio::test(start_paused = true)]
    async fn the_heartbeat_stops_renewing_when_the_attempt_ends() {
        let harness = Harness::new();
        let executor = harness.executor(vec![]);
        // 想 45 秒：心跳（30 秒一跳）至少跳一次，然后这一段落回正常终态。
        let agent = Arc::new(AgentLoop::new(
            Arc::new(SlowLlm {
                think: Duration::from_secs(45),
            }),
            harness.ledger.clone(),
            executor.clone(),
            Arc::new(harness.clock.clone()),
        ));
        let (session, run) = harness.open_run().await;

        let queue = LeaseFake::new();
        let claimed = claim_one(&queue, &run).await;

        let handler = handler(
            &harness,
            agent,
            executor,
            source_for(&harness, &session, &run),
            Arc::clone(&queue) as Arc<dyn RunQueue>,
        );
        handler.run(claimed).await.expect("跑完了");

        let view = harness.ledger.surface().runs.get(&run).cloned().unwrap();
        assert_eq!(view.status, RunState::Completed);
        let renewed_during = queue.renews();
        assert!(renewed_during >= 1, "跑着的时候要续租");
        let lease = queue.lease_of(&run).expect("续租记下了租约");

        // 再放过几个续租周期：心跳已经随这一段结束（守卫的 Drop 是 abort）。
        tokio::time::sleep(RENEW_INTERVAL * 5).await;
        assert_eq!(
            queue.renews(),
            renewed_during,
            "attempt 结束后一次都不该再续"
        );
        assert_eq!(
            queue.lease_of(&run),
            Some(lease),
            "租约表上那一次的值也不动"
        );
    }

    fn events_named(harness: &Harness, name: &str) -> usize {
        harness
            .ledger
            .events()
            .iter()
            .filter(|event| event.payload.type_name() == name)
            .count()
    }

    /// 压成了：handler 再要一段、再跑一次——同一个 Run，不回队列，`run.started` 只有一条。
    /// 一次领取里最多重装配 8 次，到了就照常跑完。
    #[tokio::test]
    async fn recompose_reassembles_without_requeue_and_caps() {
        let harness = Harness::new();
        let executor = harness.executor(vec![]);
        let mut scripts: Vec<Vec<Round>> = (0..MAX_RECOMPOSITIONS)
            .map(|n| vec![round(1, Some(&format!("第 {n} 份摘要")), vec![])])
            .collect();
        scripts.push(vec![round(1, Some("好了。"), vec![])]);
        let llm = Arc::new(ScriptedLlm::new(scripts));
        let agent = Arc::new(AgentLoop::new(
            Arc::clone(&llm) as Arc<dyn LlmClient>,
            harness.ledger.clone(),
            executor.clone(),
            Arc::new(harness.clock.clone()),
        ));
        let (session, run) = harness.open_run().await;

        let assembled = Arc::new(AtomicUsize::new(0));
        let env = harness.env(&session, &run);
        let source = {
            let assembled = Arc::clone(&assembled);
            let (session, run) = (session.clone(), run.clone());
            Arc::new(from_fn(move |_claimed, _tools| {
                assembled.fetch_add(1, Ordering::SeqCst);
                let env = env.clone();
                let (session, run) = (session.clone(), run.clone());
                async move {
                    Ok(Segment {
                        telemetry: Default::default(),
                        request: turn_request(&session, &run),
                        env,
                        budget: Budget::default(),
                        resume: None,
                        command: None,
                        // 每一段都说要压：上限之后 handler 把它摘掉。
                        compaction: Some(compaction_job(&session, &run)),
                        compactor: None,
                        session,
                        run,
                    })
                }
            })) as Arc<dyn SegmentSource>
        };
        let queue = LeaseFake::new();
        let claimed = claim_one(&queue, &run).await;
        let handler = handler(
            &harness,
            agent,
            executor,
            source,
            Arc::clone(&queue) as Arc<dyn RunQueue>,
        );
        handler.run(claimed).await.expect("跑完了");

        assert_eq!(
            assembled.load(Ordering::SeqCst),
            MAX_RECOMPOSITIONS as usize + 1,
            "压一次重新装配一次，到上限后再装配的那一段照常跑"
        );
        assert_eq!(events_named(&harness, "run.started"), 1, "没有第二次领取");
        assert_eq!(
            events_named(&harness, "context.compacted"),
            MAX_RECOMPOSITIONS as usize
        );
        assert_eq!(queue.inner.depth(), 0, "没有回队列");
        let view = harness.ledger.surface().runs.get(&run).cloned().unwrap();
        assert_eq!(view.status, RunState::Completed);
        assert_eq!(
            llm.requests.lock().unwrap().len(),
            MAX_RECOMPOSITIONS as usize + 1
        );
    }

    /// 重新装配不让预算满血复活：这一次领取里跑过的轮数从下一段的 `max_rounds` 里扣。
    #[tokio::test]
    async fn recomposing_does_not_refill_the_round_budget() {
        let harness = Harness::new();
        let executor = harness.executor(vec![Arc::new(
            crate::executor::harness::RecordingTool::bash(),
        )]);
        let mut scripts = Vec::new();
        for n in 0..3 {
            scripts.push(vec![round(
                1,
                None,
                vec![call(&format!("pc-{n}"), "bash", serde_json::json!({}))],
            )]);
            scripts.push(vec![round(1, Some("摘要"), vec![])]);
        }
        let llm = Arc::new(ScriptedLlm::new(scripts));
        let agent = Arc::new(AgentLoop::new(
            Arc::clone(&llm) as Arc<dyn LlmClient>,
            harness.ledger.clone(),
            executor.clone(),
            Arc::new(harness.clock.clone()),
        ));
        let (session, run) = harness.open_run().await;
        let env = harness.env(&session, &run);
        let source = {
            let (session, run) = (session.clone(), run.clone());
            Arc::new(from_fn(move |_claimed, _tools| {
                let env = env.clone();
                let (session, run) = (session.clone(), run.clone());
                async move {
                    // 每轮之后都问、每次都压。
                    let planner = FakePlanner::new(vec![Some(
                        komo_kernel::compaction::Reshape::Compact(compaction_job(&session, &run)),
                    )]);
                    Ok(Segment {
                        telemetry: Default::default(),
                        request: turn_request(&session, &run),
                        env,
                        budget: Budget {
                            max_rounds: Some(3),
                            ..Budget::default()
                        },
                        resume: None,
                        command: None,
                        compaction: None,
                        compactor: Some(crate::agent::Compactor {
                            pressure_tokens: Some(0),
                            ..compactor(planner)
                        }),
                        session,
                        run,
                    })
                }
            })) as Arc<dyn SegmentSource>
        };
        let handler = handler(&harness, agent, executor, source, LeaseFake::new());
        handler
            .run(Claimed {
                run: run.clone(),
                generation: 1,
            })
            .await
            .expect("跑完了");

        let view = harness.ledger.surface().runs.get(&run).cloned().unwrap();
        assert_eq!(view.status, RunState::Failed, "{view:?}");
        assert!(
            view.reason
                .as_deref()
                .is_some_and(|reason| reason.contains("总轮数预算")),
            "{view:?}"
        );
        assert_eq!(events_named(&harness, "message.assistant"), 3);
    }

    // ---- 埋点（docs/observability.md §3） ----

    fn usage(input: u64, output: u64, cache_read: u64) -> TokenUsage {
        TokenUsage {
            input: Some(input),
            output: Some(output),
            reasoning: None,
            cache_read: Some(cache_read),
            cache_write: None,
        }
    }

    /// 两轮的 Run：invoke_agent → 两个 komo.round → 第一轮 chat + execute_tool、第二轮 chat；
    /// token 属性在 chat 上，服务商没报的（reasoning、cache_write）不出现，更不是 0；
    /// 指标事件里没有任何会话 / Run / 调用 id。
    #[tokio::test]
    async fn a_two_round_run_leaves_the_span_tree_and_metrics_without_ids() {
        use crate::telemetry::capture::Capture;
        let capture = Capture::default();
        let _guard = capture.install();

        let harness = Harness::new();
        let tool = Arc::new(crate::executor::harness::RecordingTool::bash());
        let executor = harness.executor(vec![tool]);
        let mut first = round(1, None, vec![call("pc-1", "bash", serde_json::json!({}))]);
        first.usage = usage(100, 10, 80);
        let mut second = round(2, Some("好了。"), vec![]);
        second.usage = usage(130, 4, 100);
        let llm = Arc::new(ScriptedLlm::once(vec![first, second]));
        let agent = Arc::new(AgentLoop::new(
            llm,
            harness.ledger.clone(),
            executor.clone(),
            Arc::new(harness.clock.clone()),
        ));
        let (session, run) = harness.open_run().await;
        let queue = LeaseFake::new();
        let claimed = claim_one(&queue, &run).await;
        let handler = handler(
            &harness,
            agent,
            executor,
            source_for(&harness, &session, &run),
            Arc::clone(&queue) as Arc<dyn RunQueue>,
        );
        handler.run(claimed).await.expect("跑完了");

        let invoke = capture.spans("invoke_agent");
        assert_eq!(invoke.len(), 1);
        let invoke = &invoke[0];
        assert_eq!(invoke.fields["komo.run.id"], run.to_string());
        assert_eq!(invoke.fields["gen_ai.conversation.id"], session.to_string());
        assert_eq!(invoke.fields["komo.run.source"], "interactive");
        assert_eq!(invoke.fields["komo.run.outcome"], "completed");

        let rounds = capture.children(invoke, "komo.round");
        assert_eq!(rounds.len(), 2, "{:?}", capture.all_spans());
        assert_eq!(rounds[0].fields["komo.round"], "1");
        assert_eq!(rounds[1].fields["komo.round"], "2");

        let chat = capture.children(&rounds[0], "chat");
        assert_eq!(chat.len(), 1);
        let chat = &chat[0];
        assert_eq!(chat.fields["gen_ai.operation.name"], "chat");
        assert_eq!(chat.fields["komo.llm.role"], "main");
        assert_eq!(
            chat.fields["gen_ai.request.model"],
            komo_kernel::test_support::sample_model().model
        );
        assert_eq!(chat.fields["komo.llm.attempt"], "1");
        assert_eq!(chat.fields["gen_ai.usage.input_tokens"], "100");
        assert_eq!(chat.fields["gen_ai.usage.output_tokens"], "10");
        assert_eq!(chat.fields["komo.usage.cache_read_tokens"], "80");
        for absent in [
            "komo.usage.reasoning_tokens",
            "komo.usage.cache_write_tokens",
            "error.type",
        ] {
            assert!(!chat.fields.contains_key(absent), "{absent} 不该出现");
        }

        let exec = capture.children(&rounds[0], "execute_tool");
        assert_eq!(exec.len(), 1);
        assert_eq!(exec[0].fields["gen_ai.tool.name"], "bash");
        assert!(exec[0].fields.contains_key("gen_ai.tool.call.id"));
        assert_eq!(exec[0].fields["komo.tool.status"], "completed");
        assert!(exec[0].fields.contains_key("komo.tool.output_bytes"));
        assert_eq!(capture.children(&rounds[1], "chat").len(), 1);
        assert!(capture.children(&rounds[1], "execute_tool").is_empty());

        // 指标：只有低基数维度。
        let metrics = capture.all_metrics();
        assert!(!metrics.is_empty());
        let (run_text, session_text) = (run.to_string(), session.to_string());
        for event in &metrics {
            for (key, value) in &event.fields {
                assert!(
                    !matches!(
                        key.as_str(),
                        "komo.run.id" | "gen_ai.conversation.id" | "gen_ai.tool.call.id"
                    ),
                    "{key} 不能当指标维度"
                );
                assert!(
                    !value.contains(&run_text) && !value.contains(&session_text),
                    "{key}={value}"
                );
            }
        }
        let tokens = capture.metrics("histogram.gen_ai.client.token.usage");
        let kinds: Vec<&str> = tokens
            .iter()
            .map(|event| event.fields["gen_ai.token.type"].as_str())
            .collect();
        assert_eq!(
            kinds,
            [
                "input",
                "output",
                "cache_read",
                "input",
                "output",
                "cache_read"
            ]
        );
        assert_eq!(
            capture
                .metrics("histogram.gen_ai.client.operation.duration")
                .len(),
            2
        );
        let tool = capture.metrics("histogram.komo.tool.duration");
        assert_eq!(tool.len(), 1);
        assert_eq!(tool[0].fields["gen_ai.tool.name"], "bash");
        assert_eq!(tool[0].fields["komo.tool.status"], "completed");
        let rounds_metric = capture.metrics("histogram.komo.run.rounds");
        assert_eq!(rounds_metric.len(), 1);
        assert_eq!(rounds_metric[0].fields["histogram.komo.run.rounds"], "2");
        assert_eq!(rounds_metric[0].fields["komo.run.outcome"], "completed");
        assert_eq!(rounds_metric[0].fields["komo.run.source"], "interactive");
        assert_eq!(capture.metrics("histogram.komo.run.duration").len(), 1);
    }

    /// 失败的模型请求也是一个 `chat` span：带 `error.type` 与第几次尝试，没有用量；退避
    /// 重试记一次计数；这一段以 suspended 收尾。
    #[tokio::test]
    async fn a_failed_model_request_is_a_chat_span_with_an_error_type_and_counts_a_retry() {
        use crate::telemetry::capture::Capture;
        let capture = Capture::default();
        let _guard = capture.install();

        let harness = Harness::new();
        let executor = harness.executor(vec![]);
        let agent = Arc::new(AgentLoop::new(
            Arc::new(FailingLlm::at_round(LlmError::Timeout)),
            harness.ledger.clone(),
            executor.clone(),
            Arc::new(harness.clock.clone()),
        ));
        let (session, run) = harness.open_run().await;
        let queue = LeaseFake::new();
        let claimed = claim_one(&queue, &run).await;
        let handler = handler(
            &harness,
            agent,
            executor,
            source_for(&harness, &session, &run),
            Arc::clone(&queue) as Arc<dyn RunQueue>,
        );
        handler.run(claimed).await.expect("跑完了");

        let invoke = &capture.spans("invoke_agent")[0];
        assert_eq!(invoke.fields["komo.run.outcome"], "suspended");
        let round = &capture.children(invoke, "komo.round")[0];
        let chat = &capture.children(round, "chat")[0];
        assert_eq!(chat.fields["error.type"], "timeout");
        assert_eq!(chat.fields["komo.llm.attempt"], "1");
        assert!(!chat.fields.contains_key("gen_ai.usage.input_tokens"));

        let retries = capture.metrics("monotonic_counter.komo.llm.retries");
        assert_eq!(retries.len(), 1);
        assert_eq!(retries[0].fields["komo.retry.cause"], "transport");
        let duration = capture.metrics("histogram.gen_ai.client.operation.duration");
        assert_eq!(duration[0].fields["error.type"], "timeout");
        assert!(
            capture
                .metrics("histogram.gen_ai.client.token.usage")
                .is_empty()
        );
    }
}
