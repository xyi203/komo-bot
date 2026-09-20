//! Scheduler：Run 的领取、执行名额与重新入队（§8.7、§10）。
//!
//! 「启动扫描、Cron、审批回调和手动 resume 共用同一个领取入口」（§8.7），所以这里
//! **没有第二个队列**：调度器做的就是"什么时候去领"和"同时最多跑几个"，领取本身是
//! [`RunQueue::claim`]（条件更新 + 递增代次），SQL 在 store 里。
//!
//! 两个入口合成一个循环：**轮询**（周期扫描，补齐内存通知丢失的任务）与**唤醒**
//! （新 Run 提交后叫一声，§10：「触发输入在 JSONL 持久保存且 queued 已提交后再通知内存
//! 队列」）。通知丢了最多晚一个轮询周期，不会丢任务——这正是两者都要的理由。

pub mod cron;
mod zone;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use komo_kernel::traits::{LedgerError, RunQueue, StoreError};
use komo_kernel::types::ids::ExecutorId;
use komo_kernel::types::status::Claimed;
use komo_kernel::types::tool::CancelToken;
use tokio::sync::{Notify, Semaphore};

pub use cron::{CronError, CronScheduler, CronTick, Fired, SkipReason, Skipped};
pub use zone::JiffZoneResolver;

/// 领到一个 Run 之后交给谁。
///
/// 生产实现是 runtime-core 的 AgentLoop（它自己**不是** trait，§13.5）；这里留一个
/// 回调接口只是因为调度器要在没有模型、没有工具的情况下可测。
#[async_trait]
pub trait RunHandler: Send + Sync {
    /// 跑完这个 Run。**终态与挂起都由实现自己写进账本**——调度器不替它判断跑成什么样。
    async fn run(&self, claimed: Claimed) -> Result<(), HandlerError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HandlerError {
    /// 这一次没跑成，但**再领一次是有意义的**（临时故障、装配失败一类）。调度器交还
    /// 领取权。
    #[error("{0}")]
    Failed(String),
    /// handler 已经把这个 Run 停在一个不该再被领取的状态上（`waiting + intervention`、终态、
    /// 或者它自己写了挂起）。**不要交还领取权**。
    ///
    /// 它存在的理由是实测出来的：一个中间损坏的会话装配不出上下文，handler 每次都失败，
    /// 而交还领取权等于让它立刻被再领一次——"领取 → 装配失败 → 交还"在两秒里能转几十
    /// 圈，既不前进也不停下。停止受影响任务是 §8.4 的要求，停止的做法就是**不要放回去**。
    #[error("已停止：{reason}")]
    Stopped { reason: String },
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl HandlerError {
    /// 出了这个错之后，这个 Run 该不该回到队列里等下一次领取。
    ///
    /// 两种情况**不该**：handler 说它已经停了（[`HandlerError::Stopped`]），以及账本
    /// 报损坏——一个读不出来的会话下一次照样读不出来，放回去只是空转。
    pub fn returns_to_queue(&self) -> bool {
        !matches!(
            self,
            HandlerError::Stopped { .. }
                | HandlerError::Ledger(LedgerError::Corrupt(_))
                | HandlerError::Store(StoreError::Corrupt(_))
        )
    }
}

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// 多久扫一次队列。通知丢失时的兜底节奏（§10 最后一段）。
    pub poll_interval: Duration,
    /// 同时跑几个 Run。
    pub max_concurrent: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        SchedulerConfig {
            poll_interval: Duration::from_secs(1),
            // TODO(decide: 文档没有给并发上限的数。4 是一台个人机器上"不至于把模型
            // 配额和 CPU 同时打满、又不至于让一个长任务堵死其余会话"的保守取值；真要
            // 定，应当按实测改这里，而不是散在调用方。
            max_concurrent: 4,
        }
    }
}

/// 叫醒调度器的那只手。新 Run 提交后调它（§10）。
#[derive(Debug, Clone)]
pub struct Waker {
    notify: Arc<Notify>,
}

impl Waker {
    pub fn wake(&self) {
        self.notify.notify_one();
    }
}

/// 领取循环。
pub struct Scheduler {
    queue: Arc<dyn RunQueue>,
    handler: Arc<dyn RunHandler>,
    executor: ExecutorId,
    config: SchedulerConfig,
    notify: Arc<Notify>,
    permits: Arc<Semaphore>,
    handled: Arc<AtomicUsize>,
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("executor", &self.executor)
            .field("config", &self.config)
            .finish()
    }
}

impl Scheduler {
    pub fn new(
        queue: Arc<dyn RunQueue>,
        handler: Arc<dyn RunHandler>,
        executor: ExecutorId,
        config: SchedulerConfig,
    ) -> Self {
        let max = config.max_concurrent.max(1);
        Scheduler {
            queue,
            handler,
            executor,
            config: SchedulerConfig {
                max_concurrent: max,
                ..config
            },
            notify: Arc::new(Notify::new()),
            permits: Arc::new(Semaphore::new(max)),
            handled: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 提交了新 Run 就叫一声。
    pub fn waker(&self) -> Waker {
        Waker {
            notify: Arc::clone(&self.notify),
        }
    }

    /// 到目前为止交给 handler 的次数——**一个 Run 只该被交出去一次**，并发领取的测试
    /// 数的就是它。
    pub fn handled(&self) -> usize {
        self.handled.load(Ordering::SeqCst)
    }

    /// 把空着的名额填满：有多少名额就领多少个，领不到就停。
    ///
    /// 返回这一轮领走了几个。
    pub async fn fill(&self) -> Result<usize, StoreError> {
        let mut spawned = 0;
        while let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() {
            let Some(claimed) = self.queue.claim(&self.executor).await? else {
                drop(permit);
                break;
            };
            self.handled.fetch_add(1, Ordering::SeqCst);
            let queue = Arc::clone(&self.queue);
            let handler = Arc::clone(&self.handler);
            tokio::spawn(async move {
                let run = claimed.run.clone();
                let result = handler.run(claimed.clone()).await;
                if let Err(error) = result {
                    if error.returns_to_queue() {
                        // 交还名额，让它能被再领一次。**这不是"重试这次执行"**——账本上
                        // 的状态由 handler 自己写，这里只把领取权还回去。
                        tracing::warn!(run = %run, error = %error, "Run 执行失败，交还领取权");
                        if let Err(error) = queue.release(&claimed).await {
                            tracing::error!(run = %run, error = %error, "交还领取权失败");
                        }
                    } else {
                        // handler 说它已经停了（或者会话本身损坏）：**不放回队列**，
                        // 否则就是"领取 → 失败 → 交还"的空转。停止这个任务的账本状态
                        // 由 handler 负责写（`waiting + intervention`）。
                        tracing::error!(
                            run = %run,
                            error = %error,
                            "Run 已停止，不交还领取权——状态由 handler 自己写"
                        );
                    }
                }
                drop(permit);
            });
            spawned += 1;
        }
        Ok(spawned)
    }

    /// 填满名额，然后等这一批全部结束。启动扫描之后跑一次，测试里也用它。
    pub async fn run_once(&self) -> Result<usize, StoreError> {
        let spawned = self.fill().await?;
        self.wait_for_idle().await;
        Ok(spawned)
    }

    /// 等到手上一个在跑的 Run 都没有。
    ///
    /// 做法是把**所有**名额都收回来：拿得到全部名额，就说明没有任务还握着一个。
    pub async fn wait_for_idle(&self) {
        let max = self.config.max_concurrent as u32;
        if let Ok(all) = self.permits.acquire_many(max).await {
            drop(all);
        }
    }

    /// 领取循环。`shutdown` 置位后不再领新的，等手上的跑完再返回（§8.7 的正常停机）。
    pub async fn serve(&self, shutdown: CancelToken) {
        tracing::info!(executor = %self.executor, concurrency = self.config.max_concurrent, "调度器开始领取");
        while !shutdown.is_cancelled() {
            match self.fill().await {
                Ok(count) if count > 0 => {
                    tracing::debug!(count, "领到 {count} 个 Run");
                }
                Ok(_) => {}
                // 队列读不出来不是停机的理由——下一个周期再试。
                Err(error) => tracing::warn!(error = %error, "领取失败，等下一轮"),
            }
            tokio::select! {
                _ = self.notify.notified() => {}
                _ = tokio::time::sleep(self.config.poll_interval) => {}
            }
        }
        tracing::info!("停止接收新执行，等手上的收尾");
        self.wait_for_idle().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use komo_kernel::test_support::MemRunQueue;
    use komo_kernel::types::ids::RunId;

    /// 记下同时在跑几个、每个 Run 被交出去几次。
    #[derive(Debug, Default)]
    struct Recorder {
        state: Mutex<RecorderState>,
        fail_once: Mutex<Vec<RunId>>,
        /// 这些 Run 每次都以"已停止"收场——调度器不该再把它们放回队列。
        stop: Mutex<Vec<RunId>>,
        hold: Duration,
    }

    #[derive(Debug, Default)]
    struct RecorderState {
        running: usize,
        peak: usize,
        seen: Vec<RunId>,
    }

    impl Recorder {
        fn new(hold: Duration) -> Arc<Self> {
            Arc::new(Recorder {
                hold,
                ..Default::default()
            })
        }

        fn peak(&self) -> usize {
            self.state.lock().unwrap().peak
        }

        /// 此刻手上有几个在跑。
        fn running(&self) -> usize {
            self.state.lock().unwrap().running
        }

        fn seen(&self) -> Vec<RunId> {
            self.state.lock().unwrap().seen.clone()
        }
    }

    #[async_trait]
    impl RunHandler for Recorder {
        async fn run(&self, claimed: Claimed) -> Result<(), HandlerError> {
            {
                let mut state = self.state.lock().unwrap();
                state.running += 1;
                state.peak = state.peak.max(state.running);
                state.seen.push(claimed.run.clone());
            }
            tokio::time::sleep(self.hold).await;
            self.state.lock().unwrap().running -= 1;

            if self.stop.lock().unwrap().contains(&claimed.run) {
                return Err(HandlerError::Stopped {
                    reason: "会话损坏，已停成 waiting + intervention".into(),
                });
            }
            let mut failures = self.fail_once.lock().unwrap();
            if let Some(at) = failures.iter().position(|run| run == &claimed.run) {
                failures.remove(at);
                return Err(HandlerError::Failed("这次先失败".into()));
            }
            Ok(())
        }
    }

    fn scheduler(
        queue: Arc<MemRunQueue>,
        handler: Arc<Recorder>,
        max_concurrent: usize,
    ) -> Scheduler {
        Scheduler::new(
            queue,
            handler,
            ExecutorId::from_raw("exec-1"),
            SchedulerConfig {
                poll_interval: Duration::from_millis(5),
                max_concurrent,
            },
        )
    }

    #[tokio::test]
    async fn claimed_runs_are_handed_over_once_each() {
        let queue = Arc::new(MemRunQueue::new());
        for n in 0..4 {
            queue.enqueue(RunId::from_raw(format!("run-{n}")));
        }
        let handler = Recorder::new(Duration::from_millis(10));
        let scheduler = scheduler(Arc::clone(&queue), Arc::clone(&handler), 4);

        assert_eq!(scheduler.run_once().await.unwrap(), 4);
        let mut seen = handler.seen();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 4, "四个 Run 各交出去一次");
        assert_eq!(queue.depth(), 0);
    }

    #[tokio::test]
    async fn the_concurrency_limit_is_a_limit() {
        let queue = Arc::new(MemRunQueue::new());
        for n in 0..6 {
            queue.enqueue(RunId::from_raw(format!("run-{n}")));
        }
        let handler = Recorder::new(Duration::from_millis(20));
        let scheduler = scheduler(Arc::clone(&queue), Arc::clone(&handler), 2);

        assert_eq!(scheduler.run_once().await.unwrap(), 2, "一次只领两个");
        assert!(
            handler.peak() <= 2,
            "同时在跑的不超过上限：{}",
            handler.peak()
        );
        assert_eq!(queue.depth(), 4, "其余的还排着");
    }

    /// 两个执行者对着同一个队列：一个 Run 只被交出去一次（`claim` 是条件更新）。
    #[tokio::test]
    async fn two_executors_never_hand_out_the_same_run_twice() {
        let queue = Arc::new(MemRunQueue::new());
        for n in 0..8 {
            queue.enqueue(RunId::from_raw(format!("run-{n}")));
        }
        let handler = Recorder::new(Duration::from_millis(5));
        let one = scheduler(Arc::clone(&queue), Arc::clone(&handler), 4);
        let two = Scheduler::new(
            Arc::clone(&queue) as Arc<dyn RunQueue>,
            Arc::clone(&handler) as Arc<dyn RunHandler>,
            ExecutorId::from_raw("exec-2"),
            SchedulerConfig {
                poll_interval: Duration::from_millis(5),
                max_concurrent: 4,
            },
        );

        let (a, b) = tokio::join!(one.run_once(), two.run_once());
        assert_eq!(a.unwrap() + b.unwrap(), 8);

        let seen = handler.seen();
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            seen.len(),
            "没有一个 Run 被领了两次：{seen:?}"
        );
    }

    /// handler 失败 → 交还领取权，Run 回到队列里等下一次。
    #[tokio::test]
    async fn a_failed_handler_gives_the_claim_back() {
        let queue = Arc::new(MemRunQueue::new());
        let run = RunId::from_raw("run-1");
        queue.enqueue(run.clone());
        let handler = Recorder::new(Duration::from_millis(1));
        handler.fail_once.lock().unwrap().push(run.clone());

        let scheduler = scheduler(Arc::clone(&queue), Arc::clone(&handler), 1);
        scheduler.run_once().await.unwrap();
        assert_eq!(queue.depth(), 1, "失败之后名额交还，Run 还在队列里");

        scheduler.run_once().await.unwrap();
        assert_eq!(queue.depth(), 0, "第二次成功");
        assert_eq!(handler.seen().len(), 2);
    }

    /// handler 说它已经停了 → **不交还领取权**，否则就是"领取 → 失败 → 交还"的空转。
    #[tokio::test]
    async fn a_stopped_run_is_not_put_back_in_the_queue() {
        let queue = Arc::new(MemRunQueue::new());
        let run = RunId::from_raw("run-broken");
        queue.enqueue(run.clone());
        let handler = Recorder::new(Duration::from_millis(1));
        handler.stop.lock().unwrap().push(run.clone());

        let scheduler = scheduler(Arc::clone(&queue), Arc::clone(&handler), 1);
        scheduler.run_once().await.unwrap();
        assert_eq!(queue.depth(), 0, "停了就是停了，不放回队列");

        // 再扫一轮：没有东西可领，也就不会有第二次交出去。
        scheduler.run_once().await.unwrap();
        assert_eq!(handler.seen().len(), 1, "不空转");
    }

    /// 账本报损坏同理：下一次照样读不出来。
    #[test]
    fn a_corrupt_ledger_error_never_returns_to_the_queue() {
        assert!(!HandlerError::Ledger(LedgerError::Corrupt("半行".into())).returns_to_queue());
        assert!(
            !HandlerError::Stopped {
                reason: "已停止".into()
            }
            .returns_to_queue()
        );
        assert!(HandlerError::Failed("临时".into()).returns_to_queue());
        assert!(
            HandlerError::Ledger(LedgerError::Contended).returns_to_queue(),
            "写入争用下一次可能就成了"
        );
    }

    #[tokio::test]
    async fn an_empty_queue_costs_one_claim_and_no_spawn() {
        let queue = Arc::new(MemRunQueue::new());
        let handler = Recorder::new(Duration::from_millis(1));
        let scheduler = scheduler(queue, Arc::clone(&handler), 4);
        assert_eq!(scheduler.run_once().await.unwrap(), 0);
        assert_eq!(scheduler.handled(), 0);
    }

    /// 停机信号一到就不再领新的，手上的跑完才返回。
    #[tokio::test]
    async fn shutdown_stops_claiming_and_waits_for_what_is_running() {
        let queue = Arc::new(MemRunQueue::new());
        queue.enqueue(RunId::from_raw("run-1"));
        let handler = Recorder::new(Duration::from_millis(30));
        let scheduler = Arc::new(scheduler(Arc::clone(&queue), Arc::clone(&handler), 2));
        let shutdown = CancelToken::new();

        let serving = {
            let scheduler = Arc::clone(&scheduler);
            let shutdown = shutdown.clone();
            tokio::spawn(async move { scheduler.serve(shutdown).await })
        };

        tokio::time::sleep(Duration::from_millis(10)).await;
        queue.enqueue(RunId::from_raw("run-2"));
        tokio::time::sleep(Duration::from_millis(20)).await;
        shutdown.cancel();
        scheduler.waker().wake();
        serving.await.unwrap();

        assert_eq!(handler.seen().len(), 2);
        assert_eq!(handler.running(), 0, "返回时手上没有在跑的");
    }

    /// 通知丢了也不会丢任务——轮询会把它捡回来。
    #[tokio::test]
    async fn a_run_that_never_got_a_wake_up_is_still_picked_up_by_the_poll() {
        let queue = Arc::new(MemRunQueue::new());
        let handler = Recorder::new(Duration::from_millis(1));
        let scheduler = Arc::new(scheduler(Arc::clone(&queue), Arc::clone(&handler), 1));
        let shutdown = CancelToken::new();

        let serving = {
            let scheduler = Arc::clone(&scheduler);
            let shutdown = shutdown.clone();
            tokio::spawn(async move { scheduler.serve(shutdown).await })
        };
        // 入队，但**不**叫醒。
        queue.enqueue(RunId::from_raw("run-1"));
        tokio::time::sleep(Duration::from_millis(40)).await;
        shutdown.cancel();
        scheduler.waker().wake();
        serving.await.unwrap();

        assert_eq!(handler.seen().len(), 1);
    }
}
