//! `AgentLoop`：一次 Run 的一个**执行段**（§6）。
//!
//! ```text
//! LlmClient::begin_turn
//!   → TurnDriver::next            一回合一次 completion
//!   → Ledger::record_round        完整 assistant 回复 + 该轮全部调用计划，一个逻辑事件
//!   → ToolExecutor::execute_round 执行本轮全部调用（只读的可以同时在飞，§6）
//!   → 按 call_id 回传结果 → 下一轮
//! ```
//!
//! 三条硬约束写在代码的形状里：
//!
//! - **`record_round` 在 `complete` 之前。**会话消息面只认 `message.assistant`，
//!   `run.completed` 里的 `final_message` 是补读用的副本。倒过来这一轮的回答会从历史
//!   里消失，而账本看上去一切正常。
//! - **`Ask` 让出名额。**executor 报「需审批」时这一段就结束：`Ledger::suspend` 之后
//!   返回，**不在进程里等人**。审批可能一天后才从手机上来，等着的进程只是占着一个
//!   执行名额（§6、§7.4）。停下来的形状是 `waiting` + 一条 `WaitReason`——"在等谁、
//!   等到什么时候"是另一个问题，答案不在状态里。
//! - **工具失败是结果，驱动失败是终止。**普通工具失败交给模型修正；只有驱动 / LLM
//!   错误中止整轮（§6）。

use std::sync::Arc;

use komo_kernel::traits::{Clock, Ledger, LedgerError, LlmClient};
use komo_kernel::types::ids::{InterventionId, RunId, SessionId, ToolCallId};
use komo_kernel::types::model::TokenUsage;
use komo_kernel::types::status::{RunEnd, WaitReason};
use komo_kernel::types::turn::{
    AssistantRound, LlmError, Round, RoundInput, ToolCallRequest, ToolResultForModel, TurnRequest,
};

use crate::executor::{CallEnv, CallRequest, ExecError, RoundStop, ToolExecutor};

/// 一次执行段的预算（§6：Gateway 设置总轮数）。**重启不重置预算**（§8.5）——所以
/// 它由调用方从已保存的计数算出来，而不是每段从零开始。
#[derive(Debug, Clone)]
pub struct Budget {
    /// 这一段最多再跑几轮模型。
    pub max_rounds: u32,
    /// token 上限；`None` = 不设。
    pub max_tokens: Option<u64>,
    /// 这一段从第几轮编号起（续跑时接着数）。
    pub first_round: u32,
    /// 模型请求的有界退避预算（§8.5）。
    pub retry: RetryBudget,
}

/// 「普通模型超时可按有界退避重试，次数、已用预算和下次时间都持久化。**重启不重置
/// 预算**」（§8.5）。
///
/// 所以 `attempts` 是**输入**：由调用方从账本里已保存的次数给，不是每段从零开始。
#[derive(Debug, Clone)]
pub struct RetryBudget {
    /// 这个 Run 的模型请求已经重试过几次。
    pub attempts: u32,
    /// 最多几次。到了就是 failed，不是无限循环（§8.4）。
    pub max_attempts: u32,
    /// 指数退避的底：第 n 次等 `base * 2^n`。
    pub base: std::time::Duration,
}

impl Default for RetryBudget {
    fn default() -> Self {
        Self {
            attempts: 0,
            max_attempts: 5,
            // TODO(decide: 文档没有给退避的数。2s 起步、5 次封顶（累计约 1 分钟）是
            // "限流一会儿就过去了"与"别把配额耗在重试上"之间的保守取值)。
            base: std::time::Duration::from_secs(2),
        }
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            // 100 轮：24 在真活上太紧——一次"读几个文件、跑几次命令、再核对一遍"的活
            // 就能撞到顶，撞上就是一次失败的 Run（线上撞过：一个抓取整理的任务在 24 轮上
            // 停住，事情其实快做完了）。仍然是个数，不是无限；单个 Job 还能用
            // `max_rounds` 覆盖（§10）。
            max_rounds: 100,
            max_tokens: None,
            first_round: 1,
            retry: RetryBudget::default(),
        }
    }
}

/// 续跑一个已经记录过的回合：那一轮的调用还没有全部收尾。
#[derive(Debug, Clone)]
pub struct ResumedRound {
    /// 已经有结果的调用（正文由调用方从 `ToolOutputStore` 读出来）。
    pub settled: Vec<ToolResultForModel>,
    /// 还没收尾的调用，按原计划继续。
    pub pending: Vec<CallRequest>,
}

/// 一次执行段。
pub struct Segment {
    pub session: SessionId,
    pub run: RunId,
    /// 工具 Schema、系统提示、记忆注入在这里装配好（`llm` 模块的事）。
    pub request: TurnRequest,
    pub env: CallEnv,
    pub budget: Budget,
    /// 接一个已记录回合的续跑。`None` = 从模型的下一轮开始。
    pub resume: Option<ResumedRound>,
}

/// 一段跑完之后 Run 处在哪。
#[derive(Debug, Clone)]
pub enum SegmentOutcome {
    Completed {
        final_message: Option<String>,
        rounds: u32,
        usage: TokenUsage,
    },
    /// 让出了执行名额，等审批 / 等人处理 / 等退避到点（§8.4）。**状态只有一个
    /// `waiting`**，是 [`WaitReason`] 说得出在等什么。
    Suspended {
        wait: WaitReason,
        rounds: u32,
    },
    Failed {
        reason: String,
        rounds: u32,
    },
    Cancelled {
        rounds: u32,
    },
}

/// 只有账本写不进去才是错误——其余一切都有一个终态。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Exec(#[from] ExecError),
}

impl From<komo_kernel::traits::StoreError> for AgentError {
    fn from(error: komo_kernel::traits::StoreError) -> Self {
        AgentError::Exec(ExecError::Store(error))
    }
}

pub struct AgentLoop {
    llm: Arc<dyn LlmClient>,
    ledger: Arc<dyn Ledger>,
    executor: Arc<ToolExecutor>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for AgentLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLoop").finish_non_exhaustive()
    }
}

impl AgentLoop {
    pub fn new(
        llm: Arc<dyn LlmClient>,
        ledger: Arc<dyn Ledger>,
        executor: Arc<ToolExecutor>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            llm,
            ledger,
            executor,
            clock,
        }
    }

    /// 跑一段。返回时 Run 一定有一个明确去处：终态、或者一个 [`Wait`]。
    pub async fn run(&self, segment: Segment) -> Result<SegmentOutcome, AgentError> {
        let Segment {
            session: _,
            run,
            request,
            env,
            budget,
            resume,
        } = segment;

        let mut rounds = 0;

        if env.cancel.is_cancelled() {
            return self.cancel(&run, rounds).await;
        }

        let mut driver = match self.llm.begin_turn(request).await {
            Ok(driver) => driver,
            Err(error) => return self.llm_error(&run, rounds, &budget.retry, &error).await,
        };

        // 续跑：先把上一回合剩下的调用跑完，再请求下一轮模型。
        let mut input = match resume {
            None => RoundInput::First,
            Some(resumed) => {
                let mut results = resumed.settled;
                let outcome = self.executor.execute_round(resumed.pending, &env).await?;
                results.extend(outcome.results);
                if let Some(stop) = outcome.stop {
                    return self.stop(&run, rounds, stop).await;
                }
                RoundInput::ToolResults { results }
            }
        };

        loop {
            if env.cancel.is_cancelled() {
                return self.cancel(&run, rounds).await;
            }
            if rounds >= budget.max_rounds {
                return self
                    .fail_with(
                        &run,
                        rounds,
                        format!("超过总轮数预算（{}）", budget.max_rounds),
                    )
                    .await;
            }

            // 一回合一次 completion。取消和它赛跑——**每个 await 都要**。
            let round = match crate::executor::cancel::race(&env.cancel, driver.next(input)).await {
                None => return self.cancel(&run, rounds).await,
                Some(Err(error)) => {
                    return self.llm_error(&run, rounds, &budget.retry, &error).await;
                }
                Some(Ok(round)) => round,
            };
            rounds += 1;

            // 「模型回复截断或调用参数未收齐时不能开始执行」（§6）。
            if round.truncated {
                return self
                    .fail_with(
                        &run,
                        rounds,
                        "模型回复被截断，调用参数未收齐——不按半轮调用执行".into(),
                    )
                    .await;
            }

            let number = budget.first_round + rounds - 1;
            let (assistant, calls) = self.assign_ids(number, &round);
            // 完整 assistant 回复与该轮全部调用计划，一个逻辑事件（§8.3）。
            self.ledger.record_round(&run, assistant).await?;

            if calls.is_empty() {
                // 正常结束：回复已经作为 `message.assistant` 落过盘了，现在才 complete。
                self.ledger
                    .complete(
                        &run,
                        RunEnd::Completed {
                            final_message: round.text.clone(),
                            rounds,
                        },
                    )
                    .await?;
                return Ok(SegmentOutcome::Completed {
                    final_message: round.text,
                    rounds,
                    usage: driver.usage(),
                });
            }

            if let Some(limit) = budget.max_tokens
                && spent(&driver.usage()) > limit
            {
                return self
                    .fail_with(&run, rounds, format!("超过 token 预算（{limit}）"))
                    .await;
            }

            let outcome = self.executor.execute_round(calls, &env).await?;
            if let Some(stop) = outcome.stop {
                return self.stop(&run, rounds, stop).await;
            }
            input = RoundInput::ToolResults {
                results: outcome.results,
            };
        }
    }

    /// provider 的调用 → Runtime 的 `ToolCallId`。ID 是**运行时**发的，provider 的那个
    /// 另存，回放时按它配对（§8.1）。
    fn assign_ids(&self, number: u32, round: &Round) -> (AssistantRound, Vec<CallRequest>) {
        let now = self.clock.now();
        let mut requests = Vec::with_capacity(round.tool_calls.len());
        let mut recorded = Vec::with_capacity(round.tool_calls.len());
        for call in &round.tool_calls {
            let id = ToolCallId::new_at(now);
            recorded.push(ToolCallRequest {
                call_id: id.clone(),
                provider_call_id: call.provider_call_id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
                arguments_ref: None,
            });
            requests.push(CallRequest::fresh(
                id,
                call.provider_call_id.clone(),
                call.name.clone(),
                call.arguments.clone(),
            ));
        }
        let assistant = AssistantRound {
            round: number,
            text: round.text.clone(),
            text_ref: None,
            tool_calls: recorded,
            provider_blocks: round.provider_blocks.clone(),
            usage: round.usage,
        };
        (assistant, requests)
    }

    async fn stop(
        &self,
        run: &RunId,
        rounds: u32,
        stop: RoundStop,
    ) -> Result<SegmentOutcome, AgentError> {
        match stop {
            // 停在哪条调用上是**事件**的落点（`RunWaiting.call`），而这里交给
            // `Ledger::suspend` 的只有"在等一条审批"这件事：调用 ID 在 `approval_requests
            // .call_id` 上，那是权威，`suspend` 不替它猜（见 store 的实现）。
            RoundStop::Approval { approval, call } => {
                tracing::info!(run = %run, %approval, %call, "停在审批上，让出执行名额");
                let wait = WaitReason::Approval { approval };
                self.ledger.suspend(run, wait.clone()).await?;
                Ok(SegmentOutcome::Suspended { wait, rounds })
            }
            // 「结果不明」停在**一条 Intervention** 上等人核对（§7.5、§8.6）。句柄就是
            // 这个 Run：一个 Run 上最多停着一条要人判断的干预，再编一个 ID 只会和清单
            // 对不上。自由文本的理由没有地方可放（`WaitReason::Intervention` 只有句柄，
            // 事件也只有 `reason`）——那条调用的 `tool.result` 已经把"为什么不明"连同
            // 核对证据写在账上了，这里只留一条日志。
            RoundStop::Attention { reason, call } => {
                tracing::warn!(
                    run = %run,
                    %call,
                    %reason,
                    "结果不明，停在 waiting + intervention 上等人核对"
                );
                let wait = WaitReason::Intervention {
                    intervention: InterventionId::for_run(run),
                };
                self.ledger.suspend(run, wait.clone()).await?;
                Ok(SegmentOutcome::Suspended { wait, rounds })
            }
            // 委派：父 Run 让出执行名额，去等**自己派出去的那条子 Run**（§8.4 的
            // `dependency`）。它同样**不是"等人"**——没有任何人要回答什么——所以不进
            // Intervention 清单；子 Run 一进终态，父 Run 回 `Queued` 重新入队。
            //
            // 自由文本的理由在这里也没有地方可放（`WaitReason::Dependency` 只有句柄），
            // 而"父这一次调用为什么在等"本来就由账本答得出：那条 delegate 调用的计划
            // 里写着子 Run 是谁，`tool.started` 写着它已经被派出去、还没有结果。
            RoundStop::Dependency { run: child, .. } => {
                tracing::info!(run = %run, child = %child, "等子 Run 的终态，让出执行名额");
                let wait = WaitReason::Dependency { run: child };
                self.ledger.suspend(run, wait.clone()).await?;
                Ok(SegmentOutcome::Suspended { wait, rounds })
            }
            RoundStop::Cancelled => self.cancel(run, rounds).await,
        }
    }

    async fn cancel(&self, run: &RunId, rounds: u32) -> Result<SegmentOutcome, AgentError> {
        self.ledger
            .complete(run, RunEnd::Cancelled { by: None })
            .await?;
        Ok(SegmentOutcome::Cancelled { rounds })
    }

    /// 模型 / 驱动出错了：**能重试的让出名额去等退避，不能重试的当场终止**（§8.5）。
    ///
    /// 判"能不能重试、退哪一种"用 [`crate::llm::retry_cause`]——一处判断，适配器与
    /// loop 不会各有一套。结果与用量都未知（`LlmError::Unknown`）不在其中：那种情形要
    /// 保留未知标记，不能自动再来一次。
    async fn llm_error(
        &self,
        run: &RunId,
        rounds: u32,
        budget: &RetryBudget,
        error: &LlmError,
    ) -> Result<SegmentOutcome, AgentError> {
        let Some(cause) = crate::llm::retry_cause(error) else {
            return self.fail_with(run, rounds, error.to_string()).await;
        };
        let attempts = budget.attempts + 1;
        if attempts >= budget.max_attempts {
            return self
                .fail_with(
                    run,
                    rounds,
                    format!(
                        "重试预算已用完（{attempts}/{}）：{error}",
                        budget.max_attempts
                    ),
                )
                .await;
        }
        let backoff = budget.base.saturating_mul(1u32 << budget.attempts.min(16));
        let not_before = self.clock.now()
            + time::Duration::try_from(backoff).unwrap_or(time::Duration::seconds(2));
        // 自由文本的失败原因在 `WaitReason::Retry` 里没有位置（它只有次数、到点时刻与
        // 哪一种失败）——退避的进程状态由那三个字段说清楚，而"哪一次为什么退"留在日志里。
        tracing::warn!(
            run = %run,
            attempts,
            cause = cause.as_str(),
            %not_before,
            %error,
            "模型请求失败，让出执行名额去等退避"
        );
        let wait = WaitReason::Retry {
            attempts,
            not_before,
            cause,
        };
        self.ledger.suspend(run, wait.clone()).await?;
        Ok(SegmentOutcome::Suspended { wait, rounds })
    }

    async fn fail_with(
        &self,
        run: &RunId,
        rounds: u32,
        reason: String,
    ) -> Result<SegmentOutcome, AgentError> {
        self.ledger
            .complete(
                run,
                RunEnd::Failed {
                    reason: reason.clone(),
                },
            )
            .await?;
        Ok(SegmentOutcome::Failed { reason, rounds })
    }
}

/// 已知的用量之和。**未知不是零**（§8.5），所以 `None` 在这里贡献 0 只是因为它压根
/// 没进预算比较——判超预算用的是"已知花掉了多少"。
fn spent(usage: &TokenUsage) -> u64 {
    usage.input.unwrap_or(0) + usage.output.unwrap_or(0) + usage.reasoning.unwrap_or(0)
}

pub mod handler;

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) mod tests_support;
