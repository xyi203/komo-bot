//! 调度器领到一个 Run 之后交给谁（`scheduler::RunHandler` 的生产实现）。
//!
//! 职责边界抄自 `RunHandler` 的文档：**终态与挂起都由实现自己写进账本**，调度器不替
//! 它判断跑成什么样。这里就是那句话的落地——[`AgentLoop::run`] 返回时账本上已经有了
//! 终态或一个 [`Wait`](komo_kernel::types::status::Wait)，所以 handler 返回 `Ok(())`；
//! 只有**账本本身写不进去**才是 `Err`，那时调度器只 `release`，什么都没写下，下一轮
//! 重新领取。
//!
//! 装配一个执行段要三样这里看不见的东西：Session 的工作目录与已授权根（在 store
//! 里）、`TurnRequest`（系统提示、回放窗口、记忆注入，在 `llm` / `memory` 侧）、恢复
//! 位置（`recovery` 折出来的）。所以它们从一条缝进来：[`SegmentSource`]。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::traits::{Ledger, LedgerError, StoreError};
use komo_kernel::types::ids::ExecutorId;
use komo_kernel::types::status::{Claimed, Wait};
use komo_kernel::types::tool::ToolDefinition;

use crate::executor::{ExecError, ToolExecutor};
use crate::scheduler::{HandlerError, RunHandler};

use super::{AgentError, AgentLoop, Segment, SegmentOutcome};

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
        executor_id: ExecutorId,
    ) -> Self {
        Self {
            agent,
            executor,
            ledger,
            source,
            executor_id,
        }
    }
}

#[async_trait]
impl RunHandler for AgentRunHandler {
    async fn run(&self, claimed: Claimed) -> Result<(), HandlerError> {
        match self.attempt(&claimed).await {
            Ok(()) => Ok(()),
            // 「JSONL 已提交范围缺失或中间损坏时停止受影响会话的自动执行」（§8.3、
            // §8.5）。停止的做法有两半，缺一不可：**账本上说清楚**（needs_attention，
            // 操作者看得见"为什么不动了"），以及**不放回队列**——一个读不出来的会话
            // 下一次照样读不出来，交还领取权只会变成"领取 → 装配失败 → 交还"的空转。
            Err(error) if is_corrupt(&error) => {
                let reason = format!("会话损坏，已停止自动执行：{error}");
                if let Err(write) = self
                    .ledger
                    .suspend(
                        &claimed.run,
                        Wait::Attention {
                            reason: reason.clone(),
                        },
                    )
                    .await
                {
                    // 连这条都写不进去，那就只剩"不放回队列"这一半。**不要用这次写
                    // 失败盖掉损坏本身**——报告里要看到的是损坏。
                    tracing::error!(
                        run = %claimed.run,
                        error = %write,
                        "会话损坏，且 needs_attention 也写不进去"
                    );
                }
                Err(HandlerError::Stopped { reason })
            }
            Err(other) => Err(other),
        }
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

        let segment = self
            .source
            .segment(claimed, self.executor.definitions())
            .await?;

        match self.agent.run(segment).await {
            // 终态与挂起都已经在账本上了（`AgentLoop::run` 返回前一定写过一条）。
            Ok(SegmentOutcome::Completed { .. })
            | Ok(SegmentOutcome::Suspended { .. })
            | Ok(SegmentOutcome::Failed { .. })
            | Ok(SegmentOutcome::Cancelled { .. }) => Ok(()),
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
    use komo_kernel::test_support::ScriptedLlm;
    use komo_kernel::traits::LedgerError;
    use komo_kernel::types::status::RunStatus;

    #[tokio::test]
    async fn a_finished_run_leaves_its_terminal_event_on_the_ledger_and_returns_ok() {
        let harness = Harness::new();
        let executor = harness.permissive(vec![]);
        let agent = Arc::new(AgentLoop::new(
            Arc::new(ScriptedLlm::once(vec![round(1, Some("好了。"), vec![])])),
            harness.ledger.clone(),
            executor.clone(),
            Arc::new(harness.clock.clone()),
        ));
        let (session, run) = harness.open_run().await;

        let source = {
            let harness_env = harness.env(&session, &run);
            let session = session.clone();
            let run = run.clone();
            Arc::new(from_fn(move |_claimed, _tools| {
                let env = harness_env.clone();
                let session = session.clone();
                let run = run.clone();
                async move {
                    Ok(Segment {
                        request: turn_request(&session, &run),
                        env,
                        budget: Budget::default(),
                        resume: None,
                        session,
                        run,
                    })
                }
            })) as Arc<dyn SegmentSource>
        };

        let handler = AgentRunHandler::new(
            agent,
            executor,
            harness.ledger.clone(),
            source,
            ExecutorId::from_raw("exec-1"),
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
        assert_eq!(view.status, RunStatus::Completed);
        assert_eq!(view.generation, Some(1), "领取代次记下来了");
    }

    #[tokio::test]
    async fn a_source_that_cannot_assemble_fails_the_handler_without_writing_a_terminal_state() {
        let harness = Harness::new();
        let executor = harness.permissive(vec![]);
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

        let handler = AgentRunHandler::new(
            agent,
            executor,
            harness.ledger.clone(),
            source,
            ExecutorId::from_raw("exec-1"),
        );
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
            RunStatus::Running,
            "装配失败不该被写成一个执行结果"
        );
    }

    /// 一个损坏的会话：handler 说它已经停了，账本上是 `needs_attention`，
    /// 而调度器**不把它放回队列**——否则就是"领取 → 装配失败 → 交还"的空转。
    #[tokio::test]
    async fn a_corrupt_session_stops_the_run_and_never_returns_to_the_queue() {
        use komo_kernel::test_support::MemRunQueue;
        use komo_kernel::traits::RunQueue;

        let harness = Harness::new();
        let executor = harness.permissive(vec![]);
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

        let handler = AgentRunHandler::new(
            agent,
            executor,
            harness.ledger.clone(),
            source,
            ExecutorId::from_raw("exec-1"),
        );

        // 调度器那一侧：领了它，handler 失败，按 `returns_to_queue()` 决定放不放回。
        let queue = MemRunQueue::new();
        queue.enqueue(run.clone());
        let claimed = queue
            .claim(&ExecutorId::from_raw("exec-1"))
            .await
            .unwrap()
            .expect("领得到");
        assert_eq!(queue.depth(), 0);

        let error = handler.run(claimed.clone()).await.unwrap_err();
        let HandlerError::Stopped { reason } = &error else {
            panic!("{error:?}")
        };
        assert!(reason.contains("损坏"), "{reason}");
        assert!(!error.returns_to_queue(), "损坏的会话不该回到队列里");

        assert_eq!(
            harness.ledger.surface().runs.get(&run).unwrap().status,
            RunStatus::NeedsAttention,
            "账本上要说得出为什么不动了"
        );

        // 调度器照 `returns_to_queue()` 行事：不 release。队列仍然是空的。
        if error.returns_to_queue() {
            queue.release(&claimed).await.unwrap();
        }
        assert_eq!(queue.depth(), 0, "不放回去，才不会空转");
    }

    /// 存储侧的损坏走同一条路。
    #[tokio::test]
    async fn a_corrupt_output_reference_stops_the_run_too() {
        let harness = Harness::new();
        let executor = harness.permissive(vec![]);
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

        let handler = AgentRunHandler::new(
            agent,
            executor,
            harness.ledger.clone(),
            source,
            ExecutorId::from_raw("exec-1"),
        );
        let error = handler
            .run(Claimed {
                run: run.clone(),
                generation: 1,
            })
            .await
            .unwrap_err();
        assert!(matches!(error, HandlerError::Stopped { .. }), "{error:?}");
        assert!(!error.returns_to_queue());
        assert_eq!(
            harness.ledger.surface().runs.get(&run).unwrap().status,
            RunStatus::NeedsAttention
        );
    }

    /// 普通失败（不是损坏）照旧交还领取权，让它能被再领一次。
    #[tokio::test]
    async fn an_ordinary_failure_still_returns_to_the_queue() {
        let harness = Harness::new();
        let executor = harness.permissive(vec![]);
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

        let handler = AgentRunHandler::new(
            agent,
            executor,
            harness.ledger.clone(),
            source,
            ExecutorId::from_raw("exec-1"),
        );
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
            RunStatus::Running,
            "争用不是一个执行结果，也不是损坏"
        );
    }
}
