//! `AgentLoop`：一次 Run 的一个**执行段**（§6）。
//!
//! ```text
//! LlmClient::begin_turn
//!   → TurnDriver::next            一回合一次 completion
//!   → Ledger::record_round        完整 assistant 回复 + 该轮全部调用计划，一个逻辑事件
//!   → ToolExecutor::execute_round 顺序执行本轮全部调用
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
//!   执行名额（§6、§7.4）。
//! - **工具失败是结果，驱动失败是终止。**普通工具失败交给模型修正；只有驱动 / LLM
//!   错误中止整轮（§6）。

use std::sync::Arc;

use komo_kernel::traits::{Clock, Ledger, LedgerError, LlmClient};
use komo_kernel::types::ids::{RunId, SessionId, ToolCallId};
use komo_kernel::types::model::TokenUsage;
use komo_kernel::types::status::{RunEnd, Wait};
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
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_rounds: 24,
            max_tokens: None,
            first_round: 1,
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
    /// 让出了执行名额，等审批 / 等人处理。
    Suspended {
        wait: Wait,
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
            Err(error) => return self.fail(&run, rounds, &error).await,
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
                Some(Err(error)) => return self.fail(&run, rounds, &error).await,
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
            RoundStop::Approval { approval, .. } => {
                let wait = Wait::Approval {
                    approval,
                    // `Wait::Approval::call` 的类型是 `AttemptId`，而停在审批上的调用
                    // 恰恰是**还没有 attempt** 的那个（`start_call` 在放行之后）。
                    // 见报告里给编排者的那条：这个字段应当是 `ToolCallId`。
                    call: None,
                };
                self.ledger.suspend(run, wait.clone()).await?;
                Ok(SegmentOutcome::Suspended { wait, rounds })
            }
            RoundStop::Attention { reason, .. } => {
                let wait = Wait::Attention { reason };
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

    async fn fail(
        &self,
        run: &RunId,
        rounds: u32,
        error: &LlmError,
    ) -> Result<SegmentOutcome, AgentError> {
        self.fail_with(run, rounds, error.to_string()).await
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

#[cfg(test)]
mod tests;
