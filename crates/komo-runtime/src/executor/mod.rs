//! `ToolExecutor`：一轮里每个调用的公共流程（§4 末、§7、§8.5）。
//!
//! ```text
//! prepare → ExecutionPlan → Ledger::plan_call → Policy::decide
//!   ├─ Allow            → into_proof()（有匹配授权时改为 consume 换 Proof）
//!   ├─ Ask              → ApprovalRepo::create，向 loop 报「需审批」
//!   └─ Deny             → 不消费任何授权，把拒绝作为结果交回模型
//! → Ledger::start_call（首次授权消费与它同一事务）
//! → Tool::execute(ApprovedPlan)
//! → ToolOutputStore::publish → Ledger::finish_call
//! ```
//!
//! 顺序不是风格问题：`start_call` **返回之后才允许产生真实副作用**（§8.5），所以
//! `execute` 永远排在它后面；而 Deny 分支里根本没有 `consume` 的调用，这就是"审批不
//! 覆盖显式 Deny"在执行侧成立的方式。
//!
//! **恢复执行**（`ToolContext::resumed` 为 `Some`）走 §8.4 第 6 / 7 行：确定尚未执行
//! 的直接跑，started 而无结果的**先核对**，核对不出结论就交给人——不盲目重跑。
//!
//! 流式输出的写入器由 `ToolOutputStore::begin` 开、**借给** `Tool::execute`、返回后
//! 在这里 `publish`。所有权不下放，因为"先持久化输出、再追加 `tool.result`"是 §8.5
//! 的一步，而工具不知道这一步存在。
//!
//! **委派不走这条流水线的后半段**（§4）。`Operation::Delegate` 在核对梯子之前就被分流：
//! 它照样过 Policy 与审批（要审的是"允不允许把这件事派出去"），但放行之后做的事是**受理
//! 一条子 Run**、`start_call`、然后停在 `dependency` 上——没有 `execute`、没有输出发布、
//! 也没有这一轮的 `finish_call`。父子两端由**账本**接起来：子 Run 一进终态，父侧那次调用
//! 的结果就是它的终态（§8.6「可以核对目标状态」）。

pub mod cancel;
#[cfg(test)]
pub mod harness;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use komo_kernel::policy::PolicyDecision;
use komo_kernel::traits::{
    Clock, Ledger, LedgerError, RepoError, StoreError, Tool, ToolOutputStore,
};
use komo_kernel::types::chat::Principal;
use komo_kernel::types::delegate::{
    DelegateContract, DelegateSpec, SchemaMode, Validation, validate,
};
use komo_kernel::types::ids::{ApprovalId, AttemptId, RequestKey, RunId, SessionId, ToolCallId};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::plan::{
    ApprovedPlan, ConsumeIntent, EnvVersion, ExecutionPlan, Operation, PlanSource, RecoveryMode,
    Verification,
};
use komo_kernel::types::refs::{
    AttemptRef, PREVIEW_LIMIT_BYTES, PublishedOutput, ToolResultBody, ToolResultStatus,
};
use komo_kernel::types::status::{RunEnd, ToolCallState};
use komo_kernel::types::tool::{
    CancelToken, ResumedCall, ToolContext, ToolError, ToolOutput, WorkspaceRoot,
};
use komo_kernel::types::turn::{AcceptInput, Accepted, GrantUse, ToolResultForModel};

use crate::approvals::{ApprovalGate, ApprovalOutcome, ApprovalRequest};
use crate::policy::{DecisionEnv, PolicyEngine, grants_for};

use self::cancel::race;

/// 一次执行的预算（§6：Gateway 设置总轮数、活动执行时限、输出长度）。
#[derive(Debug, Clone)]
pub struct ExecutionLimits {
    /// 单次调用的活动执行时限。工具自己的超时可以更短，不能更长。
    pub call_timeout: Duration,
    /// 交给模型的结果正文上限。完整输出永远在 `ToolOutputStore` 里。
    pub model_result_bytes: usize,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            call_timeout: Duration::from_secs(300),
            model_result_bytes: PREVIEW_LIMIT_BYTES,
        }
    }
}

/// 一轮里的一个调用。
#[derive(Debug, Clone)]
pub struct CallRequest {
    pub call: ToolCallId,
    pub provider_call_id: String,
    pub tool: String,
    pub arguments: serde_json::Value,
    /// 已经落盘的计划。恢复执行沿用**同一份**——审批绑定的是它的哈希，重新 prepare
    /// 会换一个哈希，原授权就覆盖不到了（§7.4）。
    pub plan: Option<ExecutionPlan>,
    /// 这次是不是在接一次没有收尾的调用（§8.4）。`None` = 首次。
    pub resumed: Option<ResumedCall>,
    /// 这个调用停在哪条审批上（`/approve` 唤醒后带回来）。
    pub approval: Option<ApprovalId>,
}

impl CallRequest {
    /// 模型这一轮刚要求的一个调用。
    pub fn fresh(
        call: ToolCallId,
        provider_call_id: impl Into<String>,
        tool: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self {
            call,
            provider_call_id: provider_call_id.into(),
            tool: tool.into(),
            arguments,
            plan: None,
            resumed: None,
            approval: None,
        }
    }
}

/// 一轮调用共用的执行环境。由 Gateway 组装，**不能由模型参数提升**（§4）。
#[derive(Debug, Clone)]
pub struct CallEnv {
    pub session: SessionId,
    pub run: RunId,
    pub source: PlanSource,
    pub cwd: PathBuf,
    pub roots: Vec<WorkspaceRoot>,
    pub env_version: Option<EnvVersion>,
    pub principal: Option<Principal>,
    pub cancel: CancelToken,
    /// 本次 Run 固定的模型快照。委派受理子 Run 时要它：子代理不是另一个模型角色，
    /// 只是同一个模型上的一条新 Run，而 `run.accepted` 必须带上这次 Run 用的是什么。
    pub model: ModelConfig,
    /// **本 Run 是被谁派的**——子 Run 才有，普通 Run 是 `None`。
    ///
    /// 由调用方从 fold 的 `RunView.delegate` 填。它在这里是为了让"深度只有一层"是一条
    /// **能被强制的**不变量：只把 `delegate` 从子代理的工具表里摘掉是 UX，模型自己拼出
    /// 这个名字就绕过去了，而能被绕过的约束等于没有。
    pub delegated: Option<DelegateSpec>,
}

/// 这一轮为什么没跑完。
#[derive(Debug, Clone)]
pub enum RoundStop {
    /// 停在一条审批上。Run 让出执行名额，不在进程里等人（§7.4）。
    Approval {
        approval: ApprovalId,
        call: ToolCallId,
    },
    /// 需要操作者判断：结果不明、核对不出结论、或者没有可靠恢复方式（§8.6）。
    Attention { reason: String, call: ToolCallId },
    /// 在等**自己派出去的那条子 Run** 的终态（§8.4 的 `dependency`）。
    ///
    /// 它不是"等人"——没有任何人要回答什么——但父 Run 同样不能再跑：子代理的结果就是
    /// 这次调用的结果，没有它就没法继续。
    Dependency { run: RunId, call: ToolCallId },
    /// 用户取消。
    Cancelled,
}

/// 一轮执行的结果。
#[derive(Debug, Clone, Default)]
pub struct RoundOutcome {
    /// 已经有结果的调用，按 `call_id` 回传模型。
    pub results: Vec<ToolResultForModel>,
    /// 停下来了。`None` = 这一轮全部跑完。
    pub stop: Option<RoundStop>,
    /// 停下时还没轮到的调用。续跑时原样再交回来。
    pub remaining: Vec<CallRequest>,
}

/// 只有这些会中断一轮——工具自己的失败是**结果**，交给模型修正（§6）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Repo(#[from] RepoError),
}

/// 操作者对一次「结果不明」的调用能给的两个结论（§7.5 的 `verify` 条目）。
///
/// **没有"我确认副作用已发生"这一条**：操作者可能看错，而账本一旦这么记就再也纠不
/// 回来（§8.6）。`AlreadySatisfied` 说的是"核对之后外部状态已经是那个样子"，不是
/// "我相信它跑过了"——所以两个结论都要带上**看了什么**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum OperatorVerdict {
    /// 核对后目标已满足：这次调用按**已完成**收尾，正文里带证据。
    AlreadySatisfied { evidence: String },
    /// 确定没有执行：按**失败**收尾。重做是**新的一次调用**，因此照常过 Policy——
    /// 旧的那条一次性授权不会被当成重试许可（§7.4）。
    NotPerformed { evidence: String },
}

impl OperatorVerdict {
    /// 写进账本的正文。带上证据与"是谁关掉的"，事后答得出这一条凭什么算了。
    pub fn summary(&self) -> String {
        match self {
            OperatorVerdict::AlreadySatisfied { evidence } => {
                format!("操作者核对：目标已满足。证据：{evidence}")
            }
            OperatorVerdict::NotPerformed { evidence } => {
                format!("操作者核对：确定没有执行。证据：{evidence}")
            }
        }
    }

    pub fn status(&self) -> ToolResultStatus {
        match self {
            OperatorVerdict::AlreadySatisfied { .. } => ToolResultStatus::Completed,
            OperatorVerdict::NotPerformed { .. } => ToolResultStatus::Failed,
        }
    }

    /// 折算成 §8.6 的核对结论——`ResumedCall.verification` 读的就是这个形状，所以操作者
    /// 的结论与核对函数给出的结论在续跑时走同一条判断。
    pub fn as_verification(&self) -> Verification {
        match self {
            OperatorVerdict::AlreadySatisfied { evidence } => Verification::AlreadySatisfied {
                evidence: evidence.clone(),
            },
            OperatorVerdict::NotPerformed { evidence } => Verification::NotPerformed {
                evidence: evidence.clone(),
            },
        }
    }
}

pub struct ToolExecutor {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    ledger: Arc<dyn Ledger>,
    outputs: Arc<dyn ToolOutputStore>,
    approvals: ApprovalGate,
    policy: PolicyEngine,
    clock: Arc<dyn Clock>,
    limits: ExecutionLimits,
}

impl std::fmt::Debug for ToolExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolExecutor")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl ToolExecutor {
    pub fn new(
        tools: Vec<Arc<dyn Tool>>,
        ledger: Arc<dyn Ledger>,
        outputs: Arc<dyn ToolOutputStore>,
        approvals: ApprovalGate,
        policy: PolicyEngine,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tools: tools
                .into_iter()
                .map(|tool| (tool.definition().name, tool))
                .collect(),
            ledger,
            outputs,
            approvals,
            policy,
            clock,
            limits: ExecutionLimits::default(),
        }
    }

    pub fn with_limits(mut self, limits: ExecutionLimits) -> Self {
        self.limits = limits;
        self
    }

    /// 交给模型的工具 Schema。
    pub fn definitions(&self) -> Vec<komo_kernel::types::tool::ToolDefinition> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    /// **首版顺序执行同一轮的多个调用**，减少文件操作顺序歧义（§6）。
    pub async fn execute_round(
        &self,
        calls: Vec<CallRequest>,
        env: &CallEnv,
    ) -> Result<RoundOutcome, ExecError> {
        let mut outcome = RoundOutcome::default();
        let mut queue = calls.into_iter();

        while let Some(request) = queue.next() {
            if env.cancel.is_cancelled() {
                outcome.stop = Some(RoundStop::Cancelled);
                outcome.remaining = std::iter::once(request).chain(queue).collect();
                return Ok(outcome);
            }
            match self.execute_one(request.clone(), env).await? {
                CallSettlement::Result(result) => outcome.results.push(result),
                CallSettlement::Stopped { stop, result } => {
                    if let Some(result) = result {
                        outcome.results.push(result);
                    }
                    outcome.stop = Some(stop);
                    outcome.remaining = std::iter::once(request).chain(queue).collect();
                    return Ok(outcome);
                }
            }
        }
        Ok(outcome)
    }

    async fn execute_one(
        &self,
        request: CallRequest,
        env: &CallEnv,
    ) -> Result<CallSettlement, ExecError> {
        // 未知工具名：作为错误内容交回模型，**不派发任何东西**（§14 阶段 3）。
        let Some(tool) = self.tools.get(&request.tool).cloned() else {
            let known: Vec<&str> = self.tools.keys().map(String::as_str).collect();
            let message = format!(
                "没有名为 {} 的工具；可用的是：{}",
                request.tool,
                known.join("、")
            );
            return self.fail_unstarted(&request, env, message).await;
        };

        // 计划：恢复执行沿用原来那份，首次执行现做一份。
        let planning_ctx = self.context(&request, env, AttemptId::from_raw(PENDING_ATTEMPT));
        let plan = match request.plan.clone() {
            Some(plan) => plan,
            None => match tool.prepare(request.arguments.clone(), &planning_ctx).await {
                Ok(plan) => {
                    // `tool.planned`：确定尚未执行的那个可分辨状态（§8.4 第 6 行）。
                    self.ledger.plan_call(&request.call, &plan).await?;
                    plan
                }
                Err(error) => {
                    return self.fail_unstarted(&request, env, error.to_string()).await;
                }
            },
        };

        // 委派不走"工具执行"那条路，也**不走核对梯子**：对一次 delegate 调用来说，
        // "started 而无结果"的正常含义是"子 Run 还在跑"，不是"结果不明"——它的结果在
        // 我们自己的账本里（子 Run 的终态），所以 §8.6 的核对在这里有确定答案。
        if let Operation::Delegate { spec } = &plan.operation {
            return self
                .delegate(&request, env, &plan, spec, request.resumed.clone())
                .await;
        }

        // §8.4 第 6 / 7 行、§8.6：先判断是否发生，再决定是否重试。
        let mut resumed = request.resumed.clone();
        if let Some(state) = resumed.as_mut()
            && !state.is_known_not_to_have_run()
            && !safe_to_redo(&plan)
        {
            match tool.verify(&plan, &planning_ctx).await {
                Ok(verdict @ Verification::NotPerformed { .. }) => {
                    // 确定未执行且前提仍成立：可以重新执行同一原子修改。
                    state.verification = Some(verdict);
                }
                Ok(verdict @ Verification::AlreadySatisfied { .. }) => {
                    // 目标已满足**不等于**又做了一次：报告核对结论，不重跑。
                    let summary =
                        format!("核对后目标已满足，未重新执行：{}", evidence_of(&verdict));
                    self.settle_verified(
                        &request,
                        env,
                        state.previous_attempt.as_ref(),
                        ToolResultStatus::Completed,
                        &verdict,
                        &summary,
                    )
                    .await?;
                    return Ok(CallSettlement::Result(ToolResultForModel {
                        provider_call_id: request.provider_call_id.clone(),
                        call_id: request.call.clone(),
                        content: summary,
                        is_error: false,
                    }));
                }
                // 冲突 / 不出结论 / 没有核对方式：**副作用发生没发生不知道**。那条
                // 上一世的 `tool.started` 要配一条明确的 uncertain，然后交给人。
                Ok(verdict) => {
                    let summary = match &verdict {
                        Verification::Conflict { evidence } => {
                            format!("核对发现冲突：{evidence}")
                        }
                        Verification::Unknown { reason } => format!("核对不出结论：{reason}"),
                        Verification::Unavailable => format!(
                            "{} 没有可用的核对方式，不能判断上一次是否已经生效",
                            request.tool
                        ),
                        // NotPerformed 与 AlreadySatisfied 在上面两个分支里已经答过。
                        other => format!("核对结论：{other:?}"),
                    };
                    self.settle_verified(
                        &request,
                        env,
                        state.previous_attempt.as_ref(),
                        ToolResultStatus::Uncertain,
                        &verdict,
                        &summary,
                    )
                    .await?;
                    return Ok(self.attention(&request, summary));
                }
                Err(error) => {
                    // 核对本身跑不起来也是"不知道"，同样不能让 started 悬着。
                    let summary = format!("核对失败：{error}");
                    let verdict = Verification::Unknown {
                        reason: error.to_string(),
                    };
                    self.settle_verified(
                        &request,
                        env,
                        state.previous_attempt.as_ref(),
                        ToolResultStatus::Uncertain,
                        &verdict,
                        &summary,
                    )
                    .await?;
                    return Ok(self.attention(&request, summary));
                }
            }
        }

        // 放行判断。「确定没跑过」才允许再用一条已经消费过的一次性授权（§7.4）。
        let intent = if resumed
            .as_ref()
            .is_some_and(ResumedCall::is_known_not_to_have_run)
        {
            ConsumeIntent::KnownNotToHaveRun
        } else {
            ConsumeIntent::First
        };
        let (proof, grant_use) = match self.authorize(&request, &plan, env, intent).await? {
            Authorization::Proceed { proof, grant } => (proof, grant),
            Authorization::Refused(message) => {
                // 拒绝作为明确结果交回模型（§7.4）——**同时落盘**：被拒的调用一样有
                // 结论，不写下来它就永远悬着。
                return self.fail_unstarted(&request, env, message).await;
            }
            Authorization::Waiting(approval) => {
                return Ok(CallSettlement::Stopped {
                    stop: RoundStop::Approval {
                        approval,
                        call: request.call.clone(),
                    },
                    result: None,
                });
            }
        };

        // `tool.started` + 执行尝试 + 首次授权消费同一事务；**返回后才允许产生副作用**。
        let attempt = self
            .ledger
            .start_call(&request.call, &plan, grant_use)
            .await?;
        let attempt_ref = AttemptRef {
            session: env.session.clone(),
            run: env.run.clone(),
            call: request.call.clone(),
            attempt: attempt.clone(),
        };
        // 写入器**借给**工具，所有权留在这里：发布是 §8.5 的下一步（先持久化输出，
        // 再追加 `tool.result`），不在工具手里。
        let mut writer = self.outputs.begin(&attempt_ref).await?;

        let mut ctx = self.context(&request, env, attempt.clone());
        ctx.resumed = resumed;
        let started = std::time::Instant::now();
        let approved = ApprovedPlan::new(plan.clone(), proof);
        let executed = race(
            &env.cancel,
            tokio::time::timeout(
                self.limits.call_timeout,
                tool.execute(approved, &ctx, writer.as_mut()),
            ),
        )
        .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let outcome = match executed {
            None => Err(ToolError::Cancelled),
            Some(Err(_elapsed)) => Err(timeout_error(&plan, self.limits.call_timeout)),
            Some(Ok(result)) => result,
        };

        let body = result_body(&outcome);
        let status = body.status;
        let mut published = self.outputs.publish(writer, body).await?;
        published.elapsed_ms = elapsed_ms;
        published.preview = Some(preview_of(&outcome, self.limits.model_result_bytes));
        self.ledger.finish_call(&attempt, published.clone()).await?;

        let content = model_content(&published, &outcome, self.limits.model_result_bytes);
        let result = ToolResultForModel {
            provider_call_id: request.provider_call_id.clone(),
            call_id: request.call.clone(),
            content,
            is_error: status != ToolResultStatus::Completed,
        };

        match &outcome {
            Err(ToolError::Cancelled) => Ok(CallSettlement::Stopped {
                stop: RoundStop::Cancelled,
                result: Some(result),
            }),
            // 结果不明：**停止自动重试，转为结果核对**（§6、§8.6）。这段窗口要留着，
            // 不能用"重试成功"盖掉。
            Err(error) if error.is_uncertain() => Ok(CallSettlement::Stopped {
                stop: RoundStop::Attention {
                    reason: format!("{} 的结果不明：{error}", request.tool),
                    call: request.call.clone(),
                },
                result: Some(result),
            }),
            _ => Ok(CallSettlement::Result(result)),
        }
    }

    /// 一次委派的编排（§4、§8.4 的 `dependency`）。
    ///
    /// 它不走"工具执行"那条路，理由不是省事：**父这一次调用什么时候收尾，不由工具决定**。
    /// 子 Run 是账本里一条普通 Run——它可能被领取、被审批、被重启恢复，可能过了很久才结束
    /// ——父侧手里唯一那份能对上的东西是随计划进了审批绑定对象的那份 [`DelegateSpec`]，
    /// 而不是一个还活着的函数调用。
    ///
    /// 两条分支由**账本**分（`resumed` 的形状），不是由时间分：
    ///
    /// - 还没 `start_call`（首次执行，或者"受理了但还没 start 就崩了"的续跑）：先过
    ///   Policy 与审批，再受理子 Run，再 `start_call`，然后停在 `dependency` 上。
    ///   **先受理后 start 是刻意的**：受理按 `request_key` 幂等，所以"受理了但还没 start
    ///   就崩了"的续跑重走一遍拿回的是同一条子 Run，不会多派一条。
    /// - 已经 `start_call`（这次只是回来收口）：同一个键再受理一次（幂等，拿回同一条），
    ///   然后读它的终态。终态还没来就**再等一次**，不是失败。
    async fn delegate(
        &self,
        request: &CallRequest,
        env: &CallEnv,
        plan: &ExecutionPlan,
        spec: &DelegateSpec,
        resumed: Option<ResumedCall>,
    ) -> Result<CallSettlement, ExecError> {
        // 深度只有一层。这条守卫在**编排里**而不是在工具表里：把 `delegate` 从子代理的
        // 工具表摘掉是 UX，模型自己拼出这个名字就绕过去了，而能被绕过的约束等于没有。
        if let Some(parent_of_this_run) = &env.delegated {
            let message = format!(
                "子代理不能再委派：深度只有一层。你已经是被 {} 派出来跑这件事的，\
                 把完整的任务做完或说明做不到，而不是再派一条子 Run。",
                parent_of_this_run.parent
            );
            return self.fail_unstarted(request, env, message).await;
        }

        // 已经派出去过 = 上一世 `start_call` 过。`Planned` 那种"确定没跑过"的形状与之
        // 相反：它说的是"子 Run 可能还没被受理"，所以它走下面的首次路径，再受理一次。
        let in_flight = resumed
            .as_ref()
            .is_some_and(|state| !state.is_known_not_to_have_run());

        if in_flight {
            let child = self.accept_child(env, spec).await?.run;
            return self
                .collect_child(request, env, spec, resumed.as_ref(), &child)
                .await;
        }

        // "允不允许把这件事派出去"要过 Policy 与审批（§7.1）。放行在受理**之前**：
        // 没放行的委派不该在账本里留下一条没人认领的子 Run。
        let intent = if resumed
            .as_ref()
            .is_some_and(ResumedCall::is_known_not_to_have_run)
        {
            ConsumeIntent::KnownNotToHaveRun
        } else {
            ConsumeIntent::First
        };
        let grant = match self.authorize(request, plan, env, intent).await? {
            Authorization::Proceed { grant, .. } => grant,
            Authorization::Refused(message) => {
                // 与普通工具同一条规矩：结论落盘，别让这次调用悬在账本上。
                return self.fail_unstarted(request, env, message).await;
            }
            Authorization::Waiting(approval) => {
                return Ok(CallSettlement::Stopped {
                    stop: RoundStop::Approval {
                        approval,
                        call: request.call.clone(),
                    },
                    result: None,
                });
            }
        };

        let child = self.accept_child(env, spec).await?.run;
        // `tool.started` 之后子 Run 才可能被领取：它就是这次外派副作用的起点。
        self.ledger.start_call(&request.call, plan, grant).await?;
        Ok(CallSettlement::Stopped {
            stop: RoundStop::Dependency {
                run: child,
                call: request.call.clone(),
            },
            // 这次调用**没有结果**：它在等子 Run。空结果也不是结果。
            result: None,
        })
    }

    /// 受理子 Run。幂等键由**父 Run + 承载它的那次调用**决定，所以同一件委派重复受理
    /// 拿回的是同一条 Run（§8.5：受理按 `request_key` 幂等）。
    async fn accept_child(
        &self,
        env: &CallEnv,
        spec: &DelegateSpec,
    ) -> Result<Accepted, ExecError> {
        let accepted = self
            .ledger
            .accept_input(AcceptInput {
                session: env.session.clone(),
                request_key: RequestKey::new(format!("delegate:{}:{}", env.run, spec.call)),
                text: spec.task.clone(),
                // 子代理用的是**父 Run 的 source**：它不是一条新的来源，只是这次委派的
                // 延续，所以不新增 `PlanSource` 变体。它自己的每次调用照常过 Policy。
                source: env.source.clone(),
                peer: None,
                // 同一个模型快照：子代理不是另一个模型角色。
                model: env.model.clone(),
                workdir: None,
                delegate: Some(spec.clone()),
                at: self.clock.now(),
            })
            .await?;
        Ok(accepted)
    }

    /// 子 Run 的终态回到父侧那次调用上。
    ///
    /// 这是 §8.6 里"可以核对目标状态"的那一类：结果不在别人的接口上，就在我们自己的账本
    /// 里，所以"结果不明"在这里没有位置——要么子 Run 结束了，要么它还在跑。后者**再等
    /// 一次**：它可能正停在一条审批上，也可能刚被别的执行实例领走。
    async fn collect_child(
        &self,
        request: &CallRequest,
        env: &CallEnv,
        spec: &DelegateSpec,
        resumed: Option<&ResumedCall>,
        child: &RunId,
    ) -> Result<CallSettlement, ExecError> {
        let Some(end) = self.ledger.run_end(child).await? else {
            return Ok(CallSettlement::Stopped {
                stop: RoundStop::Dependency {
                    run: child.clone(),
                    call: request.call.clone(),
                },
                result: None,
            });
        };

        // 结果落回**上一世那次尝试**：父调用已经 `start_call` 过，账本里那条 `tool.started`
        // 不能永远悬着。没有它的唯一可能是账本自相矛盾——那要人看，不编一个 attempt 出来。
        let Some(attempt) = resumed.and_then(|state| state.previous_attempt.clone()) else {
            return Ok(self.attention(
                request,
                format!(
                    "委派调用 {} 已经派出去（子 Run {child}），但账上没有承载它的那次尝试",
                    request.call
                ),
            ));
        };

        let (body, content) = child_result(spec, child, &end);
        let is_error = body.status != ToolResultStatus::Completed;
        self.settle_attempt(
            AttemptRef {
                session: env.session.clone(),
                run: env.run.clone(),
                call: request.call.clone(),
                attempt,
            },
            body,
            &content,
        )
        .await?;
        Ok(CallSettlement::Result(ToolResultForModel {
            provider_call_id: request.provider_call_id.clone(),
            call_id: request.call.clone(),
            content,
            is_error,
        }))
    }

    /// §7.5：**操作者**对一次「结果不明」的调用下的结论，落到那次尝试上。
    ///
    /// 与 [`Self::settle_verified`] 是同一个动作（给那次尝试写一条 `tool.result`），
    /// 区别只在结论是谁下的：那边是工具自带的核对函数（§8.6 的第二种恢复方式，模块
    /// 已被审核过），这边是人在清单上按的键（§7.5 的 `verify` 条目）。**两条路不能
    /// 合并**：操作者的结论只有"我看过外部状态了"这一种由来，而账本上要留得下"这一条
    /// 是谁凭什么关掉的"——所以证据进正文，方法只收那两种写法。
    ///
    /// 写完这条结果，这个调用的这一轮就算收口了（`Run` 由调用方按 §8.4 重新入队）：
    /// `AlreadySatisfied` 写成 `completed`（目标已经是那个样子），`NotPerformed` 写成
    /// `failed`（重做要走新的一次调用，因此照常过 Policy）。**两种都不重跑工具**。
    pub async fn settle_by_operator(
        &self,
        session: &SessionId,
        run: &RunId,
        call: &ToolCallId,
        attempt: &AttemptId,
        verdict: OperatorVerdict,
    ) -> Result<ToolResultStatus, ExecError> {
        let attempt_ref = AttemptRef {
            session: session.clone(),
            run: run.clone(),
            call: call.clone(),
            attempt: attempt.clone(),
        };
        let summary = verdict.summary();
        let status = verdict.status();
        let body = ToolResultBody {
            status,
            result: serde_json::to_value(&verdict).unwrap_or(serde_json::Value::Null),
            error: (status != ToolResultStatus::Completed).then(|| summary.clone()),
            exit_code: None,
            artifacts: vec![],
        };
        self.settle_attempt(attempt_ref, body, &summary).await?;
        Ok(status)
    }

    /// 为**上一世那次尝试**落一条结果。
    ///
    /// §14 的目录验收要求「每个 `tool.started` 都要配一个结果或一条明确的 uncertain」。
    /// 恢复时 `verify` 给出的结论就是那次尝试的结果——不写下来，那条 started 会永远悬
    /// 着，而"悬着的 started"正是下一次恢复扫描还要再核对一遍的东西：同一次副作用会
    /// 被反复追问，却永远不落账。
    ///
    /// `previous` 为 `None` = 只 planned 过、一次尝试都没有，没有东西要收尾。
    async fn settle_verified(
        &self,
        request: &CallRequest,
        env: &CallEnv,
        previous: Option<&AttemptId>,
        status: ToolResultStatus,
        verdict: &Verification,
        summary: &str,
    ) -> Result<(), ExecError> {
        let Some(previous) = previous else {
            return Ok(());
        };
        let attempt_ref = AttemptRef {
            session: env.session.clone(),
            run: env.run.clone(),
            call: request.call.clone(),
            attempt: previous.clone(),
        };
        let body = ToolResultBody {
            status,
            result: serde_json::to_value(verdict).unwrap_or(serde_json::Value::Null),
            error: (status != ToolResultStatus::Completed).then(|| summary.to_string()),
            exit_code: None,
            artifacts: vec![],
        };
        self.settle_attempt(attempt_ref, body, summary).await
    }

    /// 把一条结果落到**指定的那次尝试**上：发布输出 → 追加 `tool.result`。
    ///
    /// 三条路共用它，因为它们做的是同一件事——给某一世的尝试写下结论：工具自带核对函数的
    /// 结论（§8.6）、操作者在清单上按的键（§7.5）、子 Run 的终态回到父侧那次调用上（§4）。
    ///
    /// `elapsed_ms` 一律留 0：那次跑了多久**我们不知道**，0 读作未知而不是"瞬间"。
    async fn settle_attempt(
        &self,
        attempt_ref: AttemptRef,
        body: ToolResultBody,
        summary: &str,
    ) -> Result<(), ExecError> {
        let writer = self.outputs.begin(&attempt_ref).await?;
        let mut published = self.outputs.publish(writer, body).await?;
        published.preview = Some(truncate(summary, PREVIEW_LIMIT_BYTES));
        self.ledger
            .finish_call(&attempt_ref.attempt, published)
            .await?;
        Ok(())
    }

    /// 一次**没有执行**的调用的结论：工具名不认识、参数准备不出来、放行被拒、子代理不能
    /// 再委派。
    ///
    /// 它们都有明确结论却**没有产生过尝试**，所以不走 [`Self::settle_attempt`]；但结论
    /// **必须落盘**（`fail_call` 建那条没有 `tool.started` 的尝试行）。只交给模型、不写
    /// 账本的话，这次调用在账本上永远悬着，而任何从账本重建的转写都带着一个没有输出的
    /// `function_call`——provider 直接 400（`No tool output found for tool call …`）。
    async fn fail_unstarted(
        &self,
        request: &CallRequest,
        env: &CallEnv,
        message: String,
    ) -> Result<CallSettlement, ExecError> {
        let attempt_ref = AttemptRef {
            session: env.session.clone(),
            run: env.run.clone(),
            call: request.call.clone(),
            attempt: AttemptId::new_at(self.clock.now()),
        };
        let body = ToolResultBody {
            status: ToolResultStatus::Failed,
            result: serde_json::Value::Null,
            error: Some(message.clone()),
            exit_code: None,
            artifacts: vec![],
        };
        let writer = self.outputs.begin(&attempt_ref).await?;
        let mut published = self.outputs.publish(writer, body).await?;
        published.preview = Some(truncate(&message, PREVIEW_LIMIT_BYTES));
        self.ledger
            .fail_call(&request.call, &attempt_ref.attempt, published)
            .await?;
        Ok(CallSettlement::Result(ToolResultForModel {
            provider_call_id: request.provider_call_id.clone(),
            call_id: request.call.clone(),
            content: message,
            is_error: true,
        }))
    }

    /// 放行梯子。Deny 分支里**没有任何一次 `consume`**。
    async fn authorize(
        &self,
        request: &CallRequest,
        plan: &ExecutionPlan,
        env: &CallEnv,
        intent: ConsumeIntent,
    ) -> Result<Authorization, ExecError> {
        let now = self.clock.now();

        // 已经停在一条审批上：拿决定，不新建请求，也不重新问人（§7.4）。
        if let Some(approval) = &request.approval {
            match self
                .approvals
                .resume_after_decision(approval, plan, intent)
                .await?
            {
                ApprovalOutcome::Approved { approval, consumed } => {
                    let proof = consumed.into_proof();
                    return Ok(Authorization::Proceed {
                        grant: Some(GrantUse {
                            approval,
                            grant: proof.grant_id().cloned(),
                        }),
                        proof,
                    });
                }
                ApprovalOutcome::Pending(record) => {
                    return Ok(Authorization::Waiting(record.approval.clone()));
                }
                ApprovalOutcome::Denied { reason, .. } => {
                    return Ok(Authorization::Refused(reason));
                }
                // 范围、计划、版本或有效期变了，或者数据库里根本没有这一行：
                // 落回正常判决，于是重新审核（§7.4、§8.5）。
                ApprovalOutcome::Stale { .. } | ApprovalOutcome::Missing { .. } => {}
            }
        }

        let grants = grants_for(self.approvals.repo().as_ref(), plan, now).await?;
        let decision_env = DecisionEnv {
            grants: &grants,
            principal: env.principal.as_ref(),
            roots: &env.roots,
            now,
        };
        let decision = self.policy.decide(plan, &decision_env);

        match decision {
            PolicyDecision::Deny { reason } => Ok(Authorization::Refused(format!(
                "被拒绝：{reason}。换一个工具做同一件事仍然受同一条规则约束。"
            ))),
            PolicyDecision::Allow { reason } => {
                // 是哪条授权放的行就消费哪条——这样 `GrantUse` 进账本，事后答得出
                // "这次是凭哪条授权跑的"。没有授权就是配置 Allow，用 Policy 的凭据。
                match self.policy.covering_grant(plan, &decision_env) {
                    Some(grant) => {
                        match self.approvals.settle(&grant.approval, plan, intent).await? {
                            ApprovalOutcome::Approved { approval, consumed } => {
                                let proof = consumed.into_proof();
                                Ok(Authorization::Proceed {
                                    grant: Some(GrantUse {
                                        approval,
                                        grant: proof.grant_id().cloned(),
                                    }),
                                    proof,
                                })
                            }
                            // 授权在这一瞬间不再可用：不要硬凑一个 Proof，退回去问人。
                            _ => self.ask(request, plan, env, reason, vec![]).await,
                        }
                    }
                    None => Ok(Authorization::Proceed {
                        proof: PolicyDecision::allow(reason)
                            .into_proof()
                            .expect("Allow 换得出 Proof"),
                        grant: None,
                    }),
                }
            }
            PolicyDecision::Ask { reason, scopes } => {
                self.ask(request, plan, env, reason, scopes).await
            }
        }
    }

    async fn ask(
        &self,
        request: &CallRequest,
        plan: &ExecutionPlan,
        env: &CallEnv,
        reason: String,
        scopes: Vec<komo_kernel::types::chat::ApprovalScope>,
    ) -> Result<Authorization, ExecError> {
        let record = self
            .approvals
            .request(ApprovalRequest {
                session: env.session.clone(),
                run: Some(env.run.clone()),
                call: Some(request.call.clone()),
                plan: plan.clone(),
                reason,
                changes: None,
                evidence: None,
                scopes,
            })
            .await?;
        Ok(Authorization::Waiting(record.approval))
    }

    fn attention(&self, request: &CallRequest, reason: String) -> CallSettlement {
        CallSettlement::Stopped {
            stop: RoundStop::Attention {
                reason,
                call: request.call.clone(),
            },
            result: None,
        }
    }

    fn context(&self, request: &CallRequest, env: &CallEnv, attempt: AttemptId) -> ToolContext {
        ToolContext {
            session: env.session.clone(),
            run: env.run.clone(),
            call: request.call.clone(),
            attempt,
            source: env.source.clone(),
            cwd: env.cwd.clone(),
            roots: env.roots.clone(),
            env_version: env.env_version.clone(),
            resumed: request.resumed.clone(),
            cancel: env.cancel.clone(),
        }
    }
}

/// `prepare` / `verify` 时还没有尝试——`start_call` 才发号。用一个认得出来的哨兵，
/// 这样账本里的 attempt ID 与它不会混。
const PENDING_ATTEMPT: &str = "attempt-not-started";

enum CallSettlement {
    Result(ToolResultForModel),
    Stopped {
        stop: RoundStop,
        result: Option<ToolResultForModel>,
    },
}

enum Authorization {
    Proceed {
        proof: komo_kernel::types::plan::Proof,
        grant: Option<GrantUse>,
    },
    Refused(String),
    Waiting(ApprovalId),
}

/// 这份计划的动作重做一遍是安全的吗。
///
/// 只有两种说得上"经过验证"：可安全重做的读取，和外部接口确实保证去重的幂等键
/// （§8.6 那张表的前两行）。其余一律先核对。
fn safe_to_redo(plan: &ExecutionPlan) -> bool {
    matches!(
        plan.recovery,
        RecoveryMode::SafeReread | RecoveryMode::IdempotencyKey { .. }
    )
}

/// 活动执行时限到了。
///
/// 读取可以说"失败了，再读一次就是"；其余动作在超时这一刻**不知道**副作用发没发生，
/// 所以是 `Uncertain` 而不是 `Failed`——后者会让模型自己再发一次，把效果做两遍。
fn timeout_error(plan: &ExecutionPlan, limit: Duration) -> ToolError {
    if safe_to_redo(plan) {
        ToolError::Timeout {
            after_secs: limit.as_secs(),
        }
    } else {
        ToolError::Uncertain {
            message: format!(
                "超过活动执行时限 {}s，副作用是否已经发生未知",
                limit.as_secs()
            ),
        }
    }
}

fn result_body(outcome: &Result<ToolOutput, ToolError>) -> ToolResultBody {
    match outcome {
        Ok(output) => ToolResultBody {
            status: output.status,
            result: output.result.clone(),
            error: None,
            exit_code: output.exit_code,
            artifacts: output.artifacts.clone(),
        },
        Err(error) => ToolResultBody {
            status: if error.is_uncertain() {
                ToolResultStatus::Uncertain
            } else {
                ToolResultStatus::Failed
            },
            result: serde_json::Value::Null,
            error: Some(error.to_string()),
            exit_code: None,
            artifacts: vec![],
        },
    }
}

/// 预览最多 [`PREVIEW_LIMIT_BYTES`]（§8.3）。
fn preview_of(outcome: &Result<ToolOutput, ToolError>, limit: usize) -> String {
    let raw = match outcome {
        Ok(output) => output.preview.clone().unwrap_or_else(|| {
            serde_json::to_string(&output.result).unwrap_or_else(|_| "（无法序列化）".into())
        }),
        Err(error) => error.to_string(),
    };
    truncate(&raw, limit.min(PREVIEW_LIMIT_BYTES))
}

/// 交给模型的正文：预览 + 完整输出的引用。
///
/// 「组装模型上下文仍遵守输出预算，超限时提供截断提示和可读取的完整文件引用」
/// （§8.3）——所以截断之后必须说得出去哪读全的。
fn model_content(
    published: &PublishedOutput,
    outcome: &Result<ToolOutput, ToolError>,
    limit: usize,
) -> String {
    let preview = preview_of(outcome, limit);
    format!("{preview}\n[完整输出：{}]", published.output.path())
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let marker = "…（已截断）";
    let room = limit.saturating_sub(marker.len());
    let mut cut = room;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{marker}", &text[..cut])
}

/// 核对结论里那句证据。
fn evidence_of(verdict: &Verification) -> &str {
    match verdict {
        Verification::AlreadySatisfied { evidence }
        | Verification::NotPerformed { evidence }
        | Verification::Conflict { evidence } => evidence,
        Verification::Unknown { reason } => reason,
        Verification::Unavailable => "没有可用的核对方式",
    }
}

/// 子 Run 的终态折算成父侧那次调用的结果。
///
/// 三条结论都要带上"哪条子 Run、它怎么结束的"：父侧的模型只看得见这条工具结果，而它随时
/// 可以去 `read` 那条 Run 的完整过程——**复验用的是父侧手里那份契约，过程由只读接口去取**
/// （§8.6）。
fn child_result(spec: &DelegateSpec, child: &RunId, end: &RunEnd) -> (ToolResultBody, String) {
    match end {
        RunEnd::Completed { final_message, .. } => {
            let text = final_message.as_deref().unwrap_or_default();
            match &spec.contract {
                // 没有契约：子代理的最后一条回复就是结果，父侧只能自己读。
                None => {
                    let mut result = outcome_map(child, end);
                    result.insert("final_message".into(), serde_json::json!(text));
                    (
                        completed_body(result),
                        format!("子 Run {child} 已完成。它的最后一条回复：\n{text}"),
                    )
                }
                Some(contract) => contract_result(contract, child, end, text),
            }
        }
        RunEnd::Failed { reason } => failed_result(
            format!("子 Run {child} 失败了：{reason}"),
            child,
            end,
            serde_json::json!({ "reason": reason }),
        ),
        RunEnd::Cancelled { by } => failed_result(
            format!("子 Run {child} 被取消，这次委派不会有结果"),
            child,
            end,
            serde_json::json!({ "by": by }),
        ),
        // 放弃与取消分开记：它不是"用户不想跑了"，而是"这件事不会再有下文了"，理由里
        // 要把这个区别说出来，否则父侧的模型只能看到一句没头没尾的失败。
        RunEnd::Abandoned { by, reason } => failed_result(
            format!(
                "子 Run {child} 被放弃，这次委派不会有结果{}",
                reason
                    .as_ref()
                    .map(|reason| format!("：{reason}"))
                    .unwrap_or_default()
            ),
            child,
            end,
            serde_json::json!({ "by": by, "reason": reason }),
        ),
    }
}

/// 有契约时：从**最后一条回复**里取 JSON，用 kernel 那个父子共用的校验器验一遍。
fn contract_result(
    contract: &DelegateContract,
    child: &RunId,
    end: &RunEnd,
    text: &str,
) -> (ToolResultBody, String) {
    match check_contract(contract, text) {
        Ok((value, validation)) => {
            let mut result = outcome_map(child, end);
            result.insert("result".into(), value.clone());
            result.insert("final_message".into(), serde_json::json!(text));
            let mut content = format!("子 Run {child} 已完成，结果符合契约：{value}");
            if !validation.ignored.is_empty() {
                // 认得但不检查的关键字要说出来：看见 `oneOf` 就当"已校验"是在撒谎，
                // 而撒谎的校验器比没有校验器更坏。
                result.insert(
                    "schema_ignored".into(),
                    serde_json::json!(validation.ignored),
                );
                content.push_str(&format!(
                    "（这些关键字没有检查，别把它们当成已经过关：{}）",
                    validation.ignored.join("、")
                ));
            }
            (completed_body(result), content)
        }
        Err(report) => match contract.mode {
            // permissive：不合规也把结果交给父侧，但**标记出来**——父侧要能把"子代理按
            // 契约给了"和"我们放行了不合规的东西"分开。
            SchemaMode::Permissive => {
                let mut result = outcome_map(child, end);
                result.insert("result".into(), serde_json::Value::Null);
                result.insert("final_message".into(), serde_json::json!(text));
                result.insert("schema_overridden".into(), serde_json::json!(true));
                result.insert("contract_report".into(), serde_json::json!(report));
                (
                    completed_body(result),
                    format!(
                        "子 Run {child} 已完成，但结果不符合契约（permissive 放行，未按契约交付）：\
                         {report}。它的最后一条回复：\n{text}"
                    ),
                )
            }
            // strict：这次委派就是失败的。原因写全，父侧要能决定"要不要用别的方式再试"。
            SchemaMode::Strict => failed_result(
                format!("子 Run {child} 的结果不符合契约（strict）：{report}"),
                child,
                end,
                serde_json::json!({ "final_message": text, "contract_report": report }),
            ),
        },
    }
}

/// 从子代理的最后一条回复里取 JSON 并按契约验一遍。
///
/// 校验器来自 kernel，**父子共用同一份代码**（§8.6）：两份实现会漂移，漂移的症状是
/// "子代理说成功、父侧说结果不合规"，而判断只能有一个出处。取 JSON 那一步与记忆提取
/// 共用（两边都容忍围栏与前后闲话），理由相同。
fn check_contract(
    contract: &DelegateContract,
    text: &str,
) -> Result<(serde_json::Value, Validation), String> {
    let Some(json) = crate::memory::parse_json_object(text) else {
        return Err("最后一条回复里找不到 JSON 对象".into());
    };
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| format!("最后一条回复里的 JSON 解不开：{error}"))?;
    let validation = validate(contract, &value);
    if validation.is_valid() {
        Ok((value, validation))
    } else {
        Err(validation.describe())
    }
}

/// 每条结果正文都带的两格：哪条子 Run、它以什么终态结束。
fn outcome_map(child: &RunId, end: &RunEnd) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert("run".into(), serde_json::json!(child));
    map.insert("status".into(), serde_json::json!(end.state().as_str()));
    map
}

fn completed_body(result: serde_json::Map<String, serde_json::Value>) -> ToolResultBody {
    ToolResultBody {
        status: ToolResultStatus::Completed,
        result: serde_json::Value::Object(result),
        error: None,
        exit_code: None,
        artifacts: vec![],
    }
}

/// 子 Run 没跑成：父侧那次调用跟着失败，理由里写明**子 Run 怎么结束的**。
fn failed_result(
    summary: String,
    child: &RunId,
    end: &RunEnd,
    extra: serde_json::Value,
) -> (ToolResultBody, String) {
    let mut result = outcome_map(child, end);
    if let serde_json::Value::Object(extra) = extra {
        result.extend(extra);
    }
    (
        ToolResultBody {
            status: ToolResultStatus::Failed,
            result: serde_json::Value::Object(result),
            error: Some(summary.clone()),
            exit_code: None,
            artifacts: vec![],
        },
        summary,
    )
}

/// 这个调用现在处在哪个状态——给恢复流程组装 [`ResumedCall`] 用。
pub fn resumed_from(
    previous_state: ToolCallState,
    previous_attempt: Option<AttemptId>,
    attempts_so_far: u32,
) -> ResumedCall {
    ResumedCall {
        previous_attempt,
        previous_state,
        attempts_so_far,
        verification: None,
    }
}

#[cfg(test)]
mod tests;
