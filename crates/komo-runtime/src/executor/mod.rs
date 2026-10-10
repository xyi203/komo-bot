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
//! 它照样过 Policy 与审批（要审的是"允不允许把这件事派出去"），但放行之后——同一轮前面
//! 在飞的调用收完之后——做的事是**受理一条子 Run**、`start_call`、然后停在 `dependency`
//! 上——没有 `execute`、没有输出发布、也没有这一轮的 `finish_call`。父子两端由**账本**接起来：子 Run 一进终态，父侧那次调用
//! 的结果就是它的终态（§8.6「可以核对目标状态」）。

pub mod cancel;
mod codemode;
#[cfg(test)]
pub mod harness;

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use serde::{Deserialize, Serialize};

use komo_kernel::compaction::{PlanUpdate, analyze_transition, format_snapshot, online_state};
use komo_kernel::events::Event;
use komo_kernel::fold::fold;
use komo_kernel::policy::PolicyDecision;
use komo_kernel::projection::{ProjectionContext, ToolResultFacts, project_views, select};
use komo_kernel::traits::{
    Clock, Ledger, LedgerError, OutputWriter, RepoError, SpawnError, StoreError, TaskSpawner, Tool,
    ToolOutputStore,
};
use komo_kernel::types::chat::Principal;
use komo_kernel::types::delegate::{
    DelegateContract, DelegateSpec, SchemaMode, Validation, validate,
};
use komo_kernel::types::ids::{
    ApprovalId, AttemptId, RequestKey, RunId, Seq, SessionId, ToolCallId,
};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::plan::{
    ApprovedPlan, ConsumeIntent, EnvVersion, ExecutionPlan, Operation, PlanSource, Proof,
    RecoveryMode, Verification,
};
use komo_kernel::types::refs::{
    AttemptRef, ContentRef, PublishedOutput, ToolResultBody, ToolResultStatus,
};
use komo_kernel::types::resource::ResourceMounts;
use komo_kernel::types::status::{RunEnd, RunState, ToolCallState};
use komo_kernel::types::surface::AgentSurface;
use komo_kernel::types::task::{FollowOutcome, TaskHandle, TaskSpec};
use komo_kernel::types::tool::{
    CancelToken, ResumedCall, ToolContext, ToolError, ToolOutput, WorkspaceRoot,
};
use komo_kernel::types::turn::{AcceptInput, Accepted, GrantUse, GrantUses, ToolResultForModel};

use crate::approvals::{ApprovalGate, ApprovalOutcome, ApprovalRequest};
use crate::policy::{DecisionEnv, PolicyEngine, grants_for};

use self::cancel::race;

/// 执行器自己的预算（§6）。
///
/// **交给模型的正文上限与活动执行时限不在这里**：它们跟着每一次执行走
/// （[`CallEnv::projection`]、[`CallEnv::call_timeout`]），因为配置是热生效的（§3）
/// ——写在这里就等于把它们钉在装配那一刻。
#[derive(Debug, Clone)]
pub struct ExecutionLimits {
    /// 同一轮里最多几条**只读**调用同时在飞（§6）。
    ///
    /// 它是并发度上限，不是正确性开关：写、审批、取消照样是屏障，唯一放开的是"两条读取
    /// 之间不必互相等"。1 = 退回一条一条跑。
    pub max_parallel_reads: usize,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            max_parallel_reads: 4,
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
    /// **这次运行允许调用哪些工具**（§4 末）。
    ///
    /// 与交给模型的 schema 是同一份：装配时由 schema 反推（`GatewaySegments::segment`），
    /// 所以"模型看见的"与"执行器认的"不可能分家。执行器不回退到全局工具目录——模型自己
    /// 拼出一个没露面的名字，得到的是"这次运行的工具集里没有它"。
    pub surface: AgentSurface,
    pub cwd: PathBuf,
    pub roots: Vec<WorkspaceRoot>,
    /// 资源命名空间的挂载点（§六）：由 Gateway 装配（skill 根、会话内容目录、能力面）。
    ///
    /// 装配早于执行，解析在实际读到它的时候做（`tools::resources`）。
    pub mounts: ResourceMounts,
    /// 工具结果怎么投影给模型（§6、§8.3）。**由 Gateway 从这条 Run 冻结的快照里填**，
    /// 与回放读的是同一份：配置热重载只影响之后受理的 Run（§3）。
    pub projection: ProjectionContext,
    /// 单次调用的活动执行时限（§6，`[execution] call_timeout_secs`）。同上，按快照热生效。
    pub call_timeout: Duration,
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
    /// 这一轮里有 `update_plan` 完成了一个**先前登记过、当时还没完成**的步骤——一个计划
    /// 边界。判据与从日志折在线状态的那一步同一个（`analyze_transition`）。
    pub plan_boundary: bool,
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
    /// `dispatch` / `follow` 的接缝（`docs/background-tasks.md`）。它是
    /// [`std::sync::OnceLock`] 而不是构造时就给的字段，理由与 `GatewayState::inbound`
    /// 一样：Gateway 那份实现自己需要一份 `Arc<GatewayState>`（建会话、`submit`），而
    /// 执行器要先造出来，`GatewayState` 才能存在——只能在那份状态造好之后再接上
    /// （[`Self::set_spawner`]）。没有装配它时两个操作按"这台 Gateway 没有接任务
    /// 分发"干净失败，不是一个悬着的调用。
    spawner: std::sync::OnceLock<Arc<dyn TaskSpawner>>,
    /// codemode 的沙箱（`docs/codemode.md`）。`None` = 这台机器上沙箱起不来，`codemode`
    /// 也不会被注册。
    codemode: Option<Arc<crate::codemode::Sandbox>>,
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
            spawner: std::sync::OnceLock::new(),
            codemode: None,
        }
    }

    pub fn with_codemode(mut self, sandbox: Option<Arc<crate::codemode::Sandbox>>) -> Self {
        self.codemode = sandbox;
        self
    }

    pub fn with_limits(mut self, limits: ExecutionLimits) -> Self {
        self.limits = limits;
        self
    }

    /// 测试用的构造方式：装配时就给一个 spawner（生产走 [`Self::set_spawner`]，见
    /// 该字段的文档）。
    #[cfg(test)]
    pub fn with_spawner(self, spawner: Arc<dyn TaskSpawner>) -> Self {
        let _ = self.spawner.set(spawner);
        self
    }

    /// 装好之后再接上任务分发的实现。只能设一次；重复设是装配错误，忽略并留一条日志
    /// （不 panic：一次重复调用不该打断整台 Gateway 的启动）。
    pub fn set_spawner(&self, spawner: Arc<dyn TaskSpawner>) {
        if self.spawner.set(spawner).is_err() {
            tracing::warn!("TaskSpawner 已经装配过一次，忽略这次重复设置");
        }
    }

    /// 全局工具目录：**发现与构造**用它（`komo skills` 的 `requires_tools` 门控也问它）。
    ///
    /// 它**不是**执行时查找的表——执行看的是 [`CallEnv::surface`]。这两件事分开正是为了
    /// 让"目录里有"不等于"这次能用"（§4 末）。
    pub fn catalog(&self) -> Vec<komo_kernel::types::tool::ToolDefinition> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    /// 交给模型的工具 Schema：**按能力面的顺序**、只列能力面里真的装着的那些。
    ///
    /// 名字在能力面里却没有实现，是装配错误（目录与能力面不同源），这里跳过并在日志里
    /// 说一声——把没有 schema 的工具交给模型没有意义，而静默地补一条假 schema 更坏。
    ///
    /// 能力面里没有 `shell` 时，`edit` / `write` 的 `then_run` 参数一并摘掉：模型看不见
    /// 一个注定被拒的参数（执行侧 `begin` 照样拒，不靠这里）。
    pub fn definitions_for(
        &self,
        surface: &AgentSurface,
    ) -> Vec<komo_kernel::types::tool::ToolDefinition> {
        let commands = self.can_run_commands(surface);
        surface
            .names()
            .iter()
            .filter_map(|name| match self.tools.get(name) {
                Some(tool) => {
                    let mut definition = tool.definition();
                    if !commands
                        && let Some(properties) = definition
                            .parameters
                            .get_mut("properties")
                            .and_then(serde_json::Value::as_object_mut)
                    {
                        properties.remove("then_run");
                    }
                    Some(definition)
                }
                None => {
                    tracing::warn!(tool = %name, "能力面里的工具没有实现，跳过它的 schema");
                    None
                }
            })
            .collect()
    }

    /// 这次运行能不能跑命令：能力面里有 `shell`，执行器也装着它。`then_run` 的放行与
    /// schema 都问它。
    fn can_run_commands(&self, surface: &AgentSurface) -> bool {
        surface.allows("shell") && self.tools.contains_key("shell")
    }

    /// 一轮里每个调用的调度（§4 末、§6）。
    ///
    /// **只读的调用可以和同一轮里后面的调用同时在飞**：够格的是 `Operation::ReadFile`
    /// 这一类计划（`read` / `rg`）里没有"上一世"要接的那条（详见
    /// [`Authorized::parallel_with_siblings`]）。其余一切（`write` / `edit` / `shell` /
    /// `python`，`delegate` / `dispatch` / `follow` / `update_plan` 这些编排调用，以及任何
    /// 要停下来的判定）都是**屏障**：屏障之前已经在飞的先收尾，再按调用顺序做它。
    ///
    /// "做它"指的是 `start_call` 及之后的一切：`begin` 只放行（可能落 `tool.planned`、
    /// 消费授权），或者给一条不必执行的调用落下结论（被拒、核对收口）；执行工具、受理子
    /// Run、派任务、转话这些副作用都排在收尾之后。所以前面在飞的那条停下来时，交回
    /// `remaining` 的屏障还没 `start_call` 过。
    ///
    /// 三条不变量不因为并发而改变：
    ///
    /// - `start_call` 返回之后才允许产生副作用（§8.5）——在飞的每一路各自遵守；
    /// - 遇到审批先收已启动调用的尾，而且**审批行在收尾之后才落**：它要是比 Run 的
    ///   挂起早一整个批次出现，答复就可能落进"Run 还没停下"的空窗（§7.4）；
    /// - 完成事件按真实完成顺序落账，交给模型的那一份按**原始调用顺序**配对。
    pub async fn execute_round(
        &self,
        calls: Vec<CallRequest>,
        env: &CallEnv,
    ) -> Result<RoundOutcome, ExecError> {
        let mut queue: VecDeque<(usize, CallRequest)> = calls.into_iter().enumerate().collect();
        let mut in_flight: InFlight<'_> = FuturesUnordered::new();
        let mut settled: Vec<(usize, ToolResultForModel)> = Vec::new();
        let mut boundary = false;

        while let Some((index, request)) = queue.pop_front() {
            if env.cancel.is_cancelled() {
                drain(&mut in_flight, &mut settled).await?;
                queue.push_front((index, request));
                return Ok(finish(Some(RoundStop::Cancelled), queue, settled, boundary));
            }

            let begin = self.begin(&request, env).await?;

            // 够格的那一类：放进在飞集合，接着看下一条。名额满了先把在飞的收完——收完
            // 名额就空出来了，于是长批一直满额在跑，而不是退化成一问一答。
            if let Begin::Ready(ready) = &begin
                && ready.parallel_with_siblings()
            {
                if in_flight.len() >= self.limits.max_parallel_reads
                    && let Some(stop) = drain(&mut in_flight, &mut settled).await?
                {
                    queue.push_front((index, request));
                    return Ok(finish(Some(stop), queue, settled, boundary));
                }
                let Begin::Ready(ready) = begin else {
                    unreachable!("上面刚判过它是 Ready")
                };
                in_flight.push(Box::pin(async move {
                    (index, self.execute_authorized(*ready, env).await)
                }));
                continue;
            }

            // 屏障：在飞的先收尾，再按顺序处理这一条。
            if let Some(stop) = drain(&mut in_flight, &mut settled).await? {
                // 在 `begin` 里就落了结论的那条（被拒、不认识、核对收口）不是"还没轮到"：
                // 结论照样交给模型，不放回去。
                match begin {
                    Begin::Settled(CallSettlement::Result(result)) => settled.push((index, result)),
                    _ => queue.push_front((index, request)),
                }
                return Ok(finish(Some(stop), queue, settled, boundary));
            }
            let settlement = match begin {
                Begin::Ready(ready) => self.execute_authorized(*ready, env).await?,
                Begin::Settled(settlement) => settlement,
                // 放行过了，在飞的也收完了：这时才 `start_call`，才受理子 Run、派任务、转话、
                // 读日志里的上一版计划。
                Begin::Orchestrate(call) => {
                    let (settlement, crossed) = self.settle_orchestrated(*call, env).await?;
                    boundary |= crossed;
                    settlement
                }
                // 需要人看一眼：**审批行现在才落**（见上面的不变量）。
                Begin::Ask(pending) => {
                    let approval = self.raise_ask(pending, env).await?;
                    let stop = RoundStop::Approval {
                        approval,
                        call: request.call.clone(),
                    };
                    queue.push_front((index, request));
                    return Ok(finish(Some(stop), queue, settled, boundary));
                }
            };
            match settlement {
                CallSettlement::Result(result) => settled.push((index, result)),
                CallSettlement::Stopped { stop, result } => {
                    settled.extend(result.map(|result| (index, result)));
                    // 停在这一条自己身上：它这一轮没收尾，所以照旧算"还没轮到"的
                    //（续跑要按账本上的形状把它重新交回来）。
                    queue.push_front((index, request));
                    return Ok(finish(Some(stop), queue, settled, boundary));
                }
            }
        }

        // 收尾：最后一并收掉还在飞的。
        let stop = drain(&mut in_flight, &mut settled).await?;
        Ok(finish(stop, queue, settled, boundary))
    }

    /// 一个调用在**执行之前**该走完的那些步：找工具 → 生成或沿用计划 → 恢复核对梯子 →
    /// 放行。几种去向见 [`Begin`]。
    ///
    /// 这几步**不和别的调用交错**：它们写账本（`plan_call`）、可能消费授权、可能要求审批。
    /// 能被搬进在飞集合的只有执行本身（[`Self::execute_authorized`]）。
    async fn begin(&self, request: &CallRequest, env: &CallEnv) -> Result<Begin, ExecError> {
        // 能力边界**先于**"有没有这个工具"（§4 末）：不在这次运行的能力面里，就等于不认识
        // 这个名字——执行器不回退到全局工具目录，所以模型自己拼出一个没露过面的名字，
        // 拿到的也只是"这次运行的工具集里没有它"。
        if !env.surface.allows(&request.tool) {
            let names = env.surface.names();
            let message = if names.is_empty() {
                format!("这次运行没有可用的工具，{} 调不了", request.tool)
            } else {
                format!(
                    "这次运行的工具集里没有 {}；可用的是：{}",
                    request.tool,
                    names.join("、")
                )
            };
            return Ok(Begin::Settled(
                self.fail_unstarted(request, env, message).await?,
            ));
        }

        // 未知工具名：作为错误内容交回模型，**不派发任何东西**（§14 阶段 3）。
        let Some(tool) = self.tools.get(&request.tool).cloned() else {
            let known: Vec<&str> = self.tools.keys().map(String::as_str).collect();
            let message = format!(
                "没有名为 {} 的工具；可用的是：{}",
                request.tool,
                known.join("、")
            );
            return Ok(Begin::Settled(
                self.fail_unstarted(request, env, message).await?,
            ));
        };

        // 计划：恢复执行沿用原来那份，首次执行现做一份。
        let planning_ctx = self.context(request, env, AttemptId::from_raw(PENDING_ATTEMPT));
        let plan = match request.plan.clone() {
            Some(plan) => plan,
            None => match tool.prepare(request.arguments.clone(), &planning_ctx).await {
                Ok(plan) => {
                    // `tool.planned`：确定尚未执行的那个可分辨状态（§8.4 第 6 行）。
                    self.ledger.plan_call(&request.call, &plan).await?;
                    plan
                }
                Err(error) => {
                    return Ok(Begin::Settled(
                        self.fail_unstarted(request, env, error.to_string()).await?,
                    ));
                }
            },
        };

        // 编排调用（delegate / dispatch / follow / update_plan）不走"工具执行"那条路，也
        // **不走核对梯子**：它们 started 而无结果时该做什么都有确定答案——delegate 的结果在
        // 我们自己的账本里（子 Run 的终态，§8.6「可以核对目标状态」），dispatch / follow 按
        // 请求键幂等重放（`docs/background-tasks.md`），update_plan 没有副作用。
        match self.orchestration(&plan.operation, env) {
            Ok(None) => {}
            Ok(Some(kind)) => return self.begin_orchestration(request, env, plan, kind).await,
            Err(message) => {
                return Ok(Begin::Settled(
                    self.fail_unstarted(request, env, message).await?,
                ));
            }
        }

        // §8.4 第 6 / 7 行、§8.6：先判断是否发生，再决定是否重试。
        let mut resumed = request.resumed.clone();
        if let Some(state) = resumed.as_mut()
            && !state.is_known_not_to_have_run()
            && !safe_to_redo(&plan)
        {
            // 带 `then_run` 的组合调用不问改动工具的核对：命令可能又改过这份文件，内容哈希
            // 对上对不上都不再说明改动那一步发生没有（§8.6）。started 而无结果就交给人。
            let verdict = if plan.then_run.is_some() {
                Ok(Verification::Unavailable)
            } else {
                tool.verify(&plan, &planning_ctx).await
            };
            match verdict {
                Ok(verdict @ Verification::NotPerformed { .. }) => {
                    // 确定未执行且前提仍成立：可以重新执行同一原子修改。
                    state.verification = Some(verdict);
                }
                Ok(verdict @ Verification::AlreadySatisfied { .. }) => {
                    // 目标已满足**不等于**又做了一次：报告核对结论，不重跑。
                    let summary =
                        format!("核对后目标已满足，未重新执行：{}", evidence_of(&verdict));
                    let recorded = self
                        .settle_verified(
                            request,
                            env,
                            state.previous_attempt.as_ref(),
                            ToolResultStatus::Completed,
                            &verdict,
                            &summary,
                        )
                        .await?;
                    // 没有上一世那次尝试就没有落盘的结论可投影：账本自相矛盾，交给人。
                    let Some(recorded) = recorded else {
                        return Ok(Begin::Settled(self.attention(
                            request,
                            format!(
                                "调用 {} 核对后目标已满足，但账上没有承载它的那次尝试",
                                request.call
                            ),
                        )));
                    };
                    return Ok(Begin::Settled(CallSettlement::Result(for_model(
                        request, env, &recorded,
                    ))));
                }
                // 冲突 / 不出结论 / 没有核对方式：**副作用发生没发生不知道**。那条
                // 上一世的 `tool.started` 要配一条明确的 uncertain，然后交给人。
                Ok(verdict) => {
                    let summary = match &verdict {
                        Verification::Conflict { evidence } => {
                            format!("核对发现冲突：{evidence}")
                        }
                        Verification::Unknown { reason } => format!("核对不出结论：{reason}"),
                        Verification::Unavailable if plan.then_run.is_some() => format!(
                            "{} 带 then_run：命令可能又改过目标，改动的核对不再可信，\
                             不能判断上一次是否已经生效",
                            request.tool
                        ),
                        Verification::Unavailable => format!(
                            "{} 没有可用的核对方式，不能判断上一次是否已经生效",
                            request.tool
                        ),
                        // NotPerformed 与 AlreadySatisfied 在上面两个分支里已经答过。
                        other => format!("核对结论：{other:?}"),
                    };
                    self.settle_verified(
                        request,
                        env,
                        state.previous_attempt.as_ref(),
                        ToolResultStatus::Uncertain,
                        &verdict,
                        &summary,
                    )
                    .await?;
                    return Ok(Begin::Settled(self.attention(request, summary)));
                }
                Err(error) => {
                    // 核对本身跑不起来也是"不知道"，同样不能让 started 悬着。
                    let summary = format!("核对失败：{error}");
                    let verdict = Verification::Unknown {
                        reason: error.to_string(),
                    };
                    self.settle_verified(
                        request,
                        env,
                        state.previous_attempt.as_ref(),
                        ToolResultStatus::Uncertain,
                        &verdict,
                        &summary,
                    )
                    .await?;
                    return Ok(Begin::Settled(self.attention(request, summary)));
                }
            }
        }

        // 组合调用的第二步是一条 shell 命令：这次运行的能力面里没有 shell（或这台执行器
        // 根本没装它），就不放行、不执行——能力面管的是"这次能做什么"，不是"模型叫哪个名字"。
        if plan.then_run.is_some() && !self.can_run_commands(&env.surface) {
            let message = format!(
                "{} 的 then_run 要跑一条命令，但这次运行的工具集里没有 shell；\
                 去掉 then_run 再调一次",
                request.tool
            );
            return Ok(Begin::Settled(
                self.fail_unstarted(request, env, message).await?,
            ));
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
        let (proof, grants) = match self.authorize(request, &plan, env, intent).await? {
            Authorization::Proceed { proof, grants } => (proof, grants),
            Authorization::Refused(message) => {
                // 拒绝作为明确结果交回模型（§7.4）——**同时落盘**：被拒的调用一样有
                // 结论，不写下来它就永远悬着。
                return Ok(Begin::Settled(
                    self.fail_unstarted(request, env, message).await?,
                ));
            }
            // 需要人看一眼：**审批行还没有落**（`execute_round` 收完在飞的再落）。
            Authorization::Ask(pending) => return Ok(Begin::Ask(pending)),
            // 停在这条**已经存在**的审批上：`/approve` 唤醒回来、决定还没写下来。
            Authorization::Waiting(approval) => {
                return Ok(Begin::Settled(CallSettlement::Stopped {
                    stop: RoundStop::Approval {
                        approval,
                        call: request.call.clone(),
                    },
                    result: None,
                }));
            }
        };

        Ok(Begin::Ready(Box::new(Authorized {
            request: request.clone(),
            tool,
            plan,
            proof,
            grants,
            resumed,
        })))
    }

    /// 一个**已经拿到凭据**的调用的执行。
    ///
    /// 只有这一段能被搬进在飞集合（[`Self::execute_round`]）：进来时放行已经结束，手里的
    /// `Proof` 就是"允许做这件事"，中途不再问任何人。
    async fn execute_authorized(
        &self,
        ready: Authorized,
        env: &CallEnv,
    ) -> Result<CallSettlement, ExecError> {
        let Authorized {
            request,
            tool,
            plan,
            proof,
            grants,
            resumed,
        } = ready;

        // `tool.started` + 执行尝试 + 首次授权消费同一事务；**返回后才允许产生副作用**。
        let attempt = self.ledger.start_call(&request.call, &plan, grants).await?;
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
        // codemode：外层照常落账，执行这一步换成沙箱里的脚本（`docs/codemode.md` §3）。
        // 组合调用：改动 → 核对 → 命令，一条结果（`tools::fusion`）。
        let execution = async {
            match (&plan.operation, approved.then_run()) {
                (Operation::Codemode { code }, _) => {
                    self.run_codemode(code, env, &ctx, writer.as_mut()).await
                }
                (_, Some(command)) => match self.tools.get("shell") {
                    Some(shell) => {
                        crate::tools::fusion::execute_fused(
                            &*tool,
                            approved,
                            &**shell,
                            command,
                            &ctx,
                            writer.as_mut(),
                        )
                        .await
                    }
                    // `begin` 已经拦过；走到这里是装配在两步之间变了。
                    None => Err(ToolError::Failed {
                        message: "then_run 要跑命令，但这台执行器没有 shell".into(),
                    }),
                },
                _ => tool.execute(approved, &ctx, writer.as_mut()).await,
            }
        };
        let executed = race(
            &env.cancel,
            tokio::time::timeout(env.call_timeout + REAP_GRACE, execution),
        )
        .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let outcome = match executed {
            None => Err(ToolError::Cancelled),
            Some(Err(_elapsed)) => Err(timeout_error(&plan, env.call_timeout)),
            Some(Ok(result)) => result,
        };

        let mut recorded = self.publish(writer, result_body(&outcome)).await?;
        recorded.published.elapsed_ms = elapsed_ms;
        self.ledger
            .finish_call(&attempt, recorded.published.clone())
            .await?;
        let result = for_model(&request, env, &recorded);
        let published = &recorded.published;
        // §48 的 `artifact_bytes`：这次尝试额外产生的产物。
        let artifact_bytes: u64 = recorded
            .artifacts
            .iter()
            .map(|artifact| artifact.size)
            .sum();

        // §47 / §48：这一份观察落了多少字节、投影给模型多少。**只进 trace，不进事件流**
        // ——它每轮都要算，而账本里那条事实只该有一条（§8.3）。投影比与 recall 次数是
        // "要不要给模型更狠的视图（Handle）"唯一说得上的证据。
        tracing::debug!(
            target: "komo::observation",
            tool = %request.tool,
            tool_output_bytes = stored_bytes(published),
            artifact_bytes,
            projected_bytes = result.content.len(),
            "observation.projected"
        );
        // §48 的 `observation_recall_count`：这一次读取读的是**我们自己落盘的观察**。
        if is_recall(&plan, env) {
            tracing::info!(
                target: "komo::observation",
                tool = %request.tool,
                paths = ?plan.paths().map(|path| path.display().to_string()).collect::<Vec<_>>(),
                "observation.recalled"
            );
        }
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

    /// 这份计划是不是一次编排调用；是的话先过只看能力的那几道（`Err` = 不合格的理由，
    /// 按没有执行过的调用落账）。
    fn orchestration(
        &self,
        operation: &Operation,
        env: &CallEnv,
    ) -> Result<Option<Orchestration>, String> {
        let spawner = || {
            self.spawner
                .get()
                .cloned()
                .ok_or_else(|| "这台 Gateway 没有接任务分发（TaskSpawner 没有装配）".to_string())
        };
        Ok(Some(match operation {
            Operation::Delegate { spec } => {
                // 深度只有一层。**第一道拦在能力面上**（子代理的 schema 里没有 `delegate`，
                // 执行器按同一份 schema 认名字，§4 末）；这里是第二道，管的是"有人把一份含
                // `delegate` 的能力面交给了子代理"——不变量不能只靠装配方记得摘掉一个名字来
                // 成立。
                if let Some(parent_of_this_run) = &env.delegated {
                    return Err(format!(
                        "子代理不能再委派：深度只有一层。你已经是被 {} 派出来跑这件事的，\
                         把完整的任务做完或说明做不到，而不是再派一条子 Run。",
                        parent_of_this_run.parent
                    ));
                }
                Orchestration::Delegate(spec.clone())
            }
            Operation::Dispatch { task, title } => Orchestration::Dispatch {
                spawner: spawner()?,
                spec: TaskSpec {
                    task: task.clone(),
                    title: title.clone(),
                },
            },
            Operation::Follow { task_id, text } => Orchestration::Follow {
                spawner: spawner()?,
                task_id: task_id.clone(),
                text: text.clone(),
            },
            Operation::UpdatePlan => Orchestration::UpdatePlan,
            _ => return Ok(None),
        }))
    }

    /// 一次编排调用的放行：续跑校验、Policy 与审批在这里，**副作用一个都不在**。
    /// `start_call` 以及之后的受理子 Run、派任务、转话、比较计划都在
    /// [`Self::settle_orchestrated`]——放进 [`Begin::Orchestrate`] 交回 `execute_round`，
    /// 在飞的读先收完再做，账本上的先后与调用顺序一致，停在前面那条上时它也还没发生。
    ///
    /// 上一世已经 `start_call` 过的那次不重新放行，同样交给收尾去接。
    async fn begin_orchestration(
        &self,
        request: &CallRequest,
        env: &CallEnv,
        plan: ExecutionPlan,
        kind: Orchestration,
    ) -> Result<Begin, ExecError> {
        // 已经开始过 = 上一世 `start_call` 过。`Planned` 那种"确定没跑过"的形状与之相反：
        // 对委派来说它是"子 Run 可能还没被受理"，所以走下面的首次路径。
        let started = request
            .resumed
            .as_ref()
            .is_some_and(|state| !state.is_known_not_to_have_run());
        let start = if started {
            OrchestrationStart::Started
        } else {
            // 续跑（§4）：目标在 `prepare` 里校验——但 `Tool::prepare` 没有账本，这里是第一次
            // 真的查得到它的地方，与深度检查同理，一旦不合格就是一次**没有执行过**的调用，
            // 按 `fail_call` 落账（不靠"先查后写"：受理那一步的 db 事务里还有第二道，§4）。
            if let Orchestration::Delegate(spec) = &kind
                && let Some(message) = self.validate_resume(env, spec).await?
            {
                return Ok(Begin::Settled(
                    self.fail_unstarted(request, env, message).await?,
                ));
            }

            // "允不允许把这件事派出去"同样要过 Policy 与审批（§7.1）。放行在受理**之前**：
            // 没放行的委派不该在账本里留下一条没人认领的子 Run。
            let intent = if request
                .resumed
                .as_ref()
                .is_some_and(ResumedCall::is_known_not_to_have_run)
            {
                ConsumeIntent::KnownNotToHaveRun
            } else {
                ConsumeIntent::First
            };
            match self.authorize(request, &plan, env, intent).await? {
                Authorization::Proceed { grants, .. } => OrchestrationStart::Fresh(grants),
                Authorization::Refused(message) => {
                    // 与普通工具同一条规矩：结论落盘，别让这次调用悬在账本上。
                    return Ok(Begin::Settled(
                        self.fail_unstarted(request, env, message).await?,
                    ));
                }
                Authorization::Ask(pending) => return Ok(Begin::Ask(pending)),
                Authorization::Waiting(approval) => {
                    return Ok(Begin::Settled(CallSettlement::Stopped {
                        stop: RoundStop::Approval {
                            approval,
                            call: request.call.clone(),
                        },
                        result: None,
                    }));
                }
            }
        };
        Ok(Begin::Orchestrate(Box::new(Orchestrated {
            request: request.clone(),
            plan,
            start,
            kind,
        })))
    }

    /// 一次放行过的编排调用的收尾：`execute_round` 收完在飞的调用之后才走到这里。第二个
    /// 返回值是 update_plan 的计划边界，别的编排调用恒为 `false`。
    ///
    /// dispatch / follow **不等**任何东西：`TaskSpawner` 一返回就收尾——这正是它们存在的
    /// 理由，等任务跑完又会把派它的会话堵住（同一会话严格串行）。幂等靠
    /// `request_key = dispatch|follow:{run}:{call}`：首次执行与"上一世已经 `start_call`
    /// 过、这一世只是回来收口"的续跑调的是同一个键，`TaskSpawner` 内部走的是
    /// `GatewayState::submit` 那条按请求键去重的路（§8.5），重放拿回同一个任务会话。
    async fn settle_orchestrated(
        &self,
        call: Orchestrated,
        env: &CallEnv,
    ) -> Result<(CallSettlement, bool), ExecError> {
        let Orchestrated {
            request,
            plan,
            start,
            kind,
        } = call;
        if let Orchestration::Delegate(spec) = &kind {
            let settlement = self
                .settle_delegate(&request, env, &plan, spec, start)
                .await?;
            return Ok((settlement, false));
        }
        let attempt = match start {
            OrchestrationStart::Fresh(grants) => {
                self.ledger.start_call(&request.call, &plan, grants).await?
            }
            OrchestrationStart::Started => {
                let previous = request
                    .resumed
                    .as_ref()
                    .and_then(|state| state.previous_attempt.clone());
                let Some(attempt) = previous else {
                    let reason = format!(
                        "{} 调用 {} 已经开始过，但账上没有承载它的那次尝试",
                        request.tool, request.call
                    );
                    return Ok((self.attention(&request, reason), false));
                };
                attempt
            }
        };
        let settlement = match kind {
            Orchestration::Dispatch { spawner, spec } => {
                let key = RequestKey::new(format!("dispatch:{}:{}", env.run, request.call));
                let outcome = spawner.spawn(&env.run, key, spec).await;
                self.settle_dispatch(&request, env, attempt, outcome)
                    .await?
            }
            Orchestration::Follow {
                spawner,
                task_id,
                text,
            } => {
                let key = RequestKey::new(format!("follow:{}:{}", env.run, request.call));
                let outcome = spawner.follow(&env.run, key, &task_id, &text).await;
                self.settle_follow(&request, env, attempt, outcome).await?
            }
            Orchestration::UpdatePlan => {
                return self.settle_plan(&request, env, &plan, attempt).await;
            }
            Orchestration::Delegate(_) => unreachable!("委派在上面已经收尾"),
        };
        Ok((settlement, false))
    }

    /// 一次委派的收尾（§4、§8.4 的 `dependency`）。
    ///
    /// 它不走"工具执行"那条路，理由不是省事：**父这一次调用什么时候收尾，不由工具决定**。
    /// 子 Run 是账本里一条普通 Run——它可能被领取、被审批、被重启恢复，可能过了很久才结束
    /// ——父侧手里唯一那份能对上的东西是随计划进了审批绑定对象的那份 [`DelegateSpec`]，
    /// 而不是一个还活着的函数调用。
    ///
    /// 两条分支由**账本**分（`resumed` 的形状），不是由时间分：
    ///
    /// - 还没 `start_call`（首次执行，或者"受理了但还没 start 就崩了"的续跑）：放行过了，
    ///   受理子 Run，再 `start_call`，然后停在 `dependency` 上。**先受理后 start 是刻意的**：
    ///   受理按 `request_key` 幂等，所以"受理了但还没 start 就崩了"的续跑重走一遍拿回的是
    ///   同一条子 Run，不会多派一条。
    /// - 已经 `start_call`（这次只是回来收口）：同一个键再受理一次（幂等，拿回同一条），
    ///   然后读它的终态。终态还没来就**再等一次**，不是失败。
    async fn settle_delegate(
        &self,
        request: &CallRequest,
        env: &CallEnv,
        plan: &ExecutionPlan,
        spec: &DelegateSpec,
        start: OrchestrationStart,
    ) -> Result<CallSettlement, ExecError> {
        let child = self.accept_child(env, spec).await?.run;
        let grants = match start {
            OrchestrationStart::Started => {
                return self
                    .collect_child(request, env, spec, request.resumed.as_ref(), &child)
                    .await;
            }
            OrchestrationStart::Fresh(grants) => grants,
        };
        // `tool.started` 之后子 Run 才可能被领取：它就是这次外派副作用的起点。
        self.ledger.start_call(&request.call, plan, grants).await?;
        Ok(CallSettlement::Stopped {
            stop: RoundStop::Dependency {
                run: child,
                call: request.call.clone(),
            },
            // 这次调用**没有结果**：它在等子 Run。空结果也不是结果。
            result: None,
        })
    }

    /// 续跑目标合不合格（§4）：`Some(reason)` = 不合格，理由交给模型；`None` = 可以接着
    /// 走放行梯子。没有 `resume` 参数时直接放行（`None`）。
    ///
    /// 四条都在这里判：① 同一 Session、且是一条子 Run；② 已经终态，且终态是 completed
    /// 或 failed；③ 在最新一个 `conversation.boundary` 之后；④ 是这条线的末端（没有别的
    /// 子 Run 在 `resumes` 它，不论那条什么状态），理由里写出末端是哪一条。
    ///
    /// 这里要把整个 Session 的日志读一遍再 `fold`——线是从 JSONL 里折出来的
    /// （`run.accepted.delegate.resumes`），不查 state.db（§8.3 的内容权威）。它只在**首次**
    /// 执行这次委派时跑一遍，不在每次续跑收口时重复。
    async fn validate_resume(
        &self,
        env: &CallEnv,
        spec: &DelegateSpec,
    ) -> Result<Option<String>, ExecError> {
        let Some(target) = spec.resumes.clone() else {
            return Ok(None);
        };
        let events = self.events_of(&env.session).await?;
        let surface = fold(&events);

        let Some(view) = surface.runs.get(&target) else {
            return Ok(Some(format!(
                "接不上 {target}：这条 Run 不在同一个 Session 里，或者账本里根本没有它。"
            )));
        };
        if view.delegate.is_none() {
            return Ok(Some(format!(
                "{target} 是主对话的 Run，不是子代理，没有什么线可以接。"
            )));
        }
        if !view.status.is_terminal() {
            return Ok(Some(format!(
                "{target} 还没有结束（现在是 {}），续不了一条还在跑的子 Run。",
                state_word(view.status)
            )));
        }
        if !matches!(view.status, RunState::Completed | RunState::Failed) {
            return Ok(Some(format!(
                "{target} 是 {} 结束的：cancelled / abandoned 是操作者说过\"这件事到此为止\"，\
                 不能续。",
                state_word(view.status)
            )));
        }
        // 在最新一个 conversation.boundary 之后：`run.accepted` 那条消息进不进回放窗口
        // 就是判据——边界之前的那条线属于上一段对话，父的窗口里已经看不到它。
        let after_boundary = surface
            .replay()
            .iter()
            .any(|message| message.run.as_ref() == Some(&target));
        if !after_boundary {
            return Ok(Some(format!(
                "{target} 在最新一次 /new 之前，属于上一段对话，接不上。"
            )));
        }
        if let Some(resumer) = find_resumer(&surface, &target) {
            let tail = tail_of(&surface, &resumer);
            return Ok(Some(format!(
                "{target} 已经被续跑过：这条线现在的末端是 {tail}，线只往后接，续到 {tail} \
                 才行；想另起一条就别填 resume。"
            )));
        }
        Ok(None)
    }

    /// 读完一个 Session 的全部事件（分页）。`validate_resume` 用它折出这条线。
    async fn events_of(&self, session: &SessionId) -> Result<Vec<Event>, ExecError> {
        let mut all = Vec::new();
        let mut from = Seq::ZERO;
        loop {
            let batch = self.ledger.read(session, from, 0).await?;
            if batch.events.is_empty() {
                return Ok(all);
            }
            for event in batch.events {
                from = from.max(event.seq);
                all.push(event);
            }
            if batch.next.is_none() {
                return Ok(all);
            }
        }
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
                // 子 Run 的身份与能力**由父侧继承**（同一个 Agent、同一份工作目录、同样的
                // 工具减去 `delegate`）。执行器手里只有父 Run 的 `CallEnv`，没有 Profile、
                // 工作目录与指令正文，所以这里不编一份：装配那一步按父 Run 的快照继承
                // （`service::segment`），与"旧行没有归属"走的是同一条兜底路。
                snapshot: None,
                // 子代理是一段真的模型对话，记忆提取照常（§9.3）；跳过只留给命令
                // Job——那种 Run 从不发模型请求，没有对话可提取（§10）。
                skip_memory: false,
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

        let recorded = self
            .settle_attempt(
                AttemptRef {
                    session: env.session.clone(),
                    run: env.run.clone(),
                    call: request.call.clone(),
                    attempt,
                },
                child_result(spec, child, &end),
            )
            .await?;
        Ok(CallSettlement::Result(for_model(request, env, &recorded)))
    }

    /// `dispatch` 的结论：`TaskHandle` 折成"已派出 #短号：标题"，`SpawnError` 折成一句
    /// 失败原因。
    async fn settle_dispatch(
        &self,
        request: &CallRequest,
        env: &CallEnv,
        attempt: AttemptId,
        outcome: Result<TaskHandle, SpawnError>,
    ) -> Result<CallSettlement, ExecError> {
        let (result, content, is_error) = match outcome {
            Ok(handle) => {
                let content = format!("已派出 #{}：{}", handle.short_id, handle.title);
                (
                    serde_json::json!({
                        "session": handle.session.to_string(),
                        "short_id": handle.short_id,
                        "title": handle.title,
                    }),
                    content,
                    false,
                )
            }
            Err(error) => {
                let content = format!("派任务失败：{error}");
                (serde_json::json!({ "error": content }), content, true)
            }
        };
        self.settle_orchestration(request, env, attempt, result, content, is_error)
            .await
    }

    /// `follow` 的结论：`FollowOutcome` 折成"已转给 #短号"（或"正在跑，排在后面"），
    /// `SpawnError` 折成一句失败原因（短号解析不到 / 有歧义 / 提交失败）。
    async fn settle_follow(
        &self,
        request: &CallRequest,
        env: &CallEnv,
        attempt: AttemptId,
        outcome: Result<FollowOutcome, SpawnError>,
    ) -> Result<CallSettlement, ExecError> {
        let (result, content, is_error) = match outcome {
            Ok(followed) => {
                let content = if followed.queued_behind {
                    format!("#{} 正在跑，这句排在它后面", followed.short_id)
                } else {
                    format!("已转给 #{}", followed.short_id)
                };
                (
                    serde_json::json!({
                        "session": followed.session.to_string(),
                        "short_id": followed.short_id,
                        "queued_behind": followed.queued_behind,
                    }),
                    content,
                    false,
                )
            }
            Err(error) => {
                let content = format!("转不过去：{error}");
                (serde_json::json!({ "error": content }), content, true)
            }
        };
        self.settle_orchestration(request, env, attempt, result, content, is_error)
            .await
    }

    /// `update_plan` 的收尾：上一版计划从**这条 Run 的日志**折出来（与在线状态同一个
    /// fold），和这一版比较；结果是这一版的快照加上给模型的提醒。第二个返回值是"完成了
    /// 一个先前登记过的步骤"——计划边界。
    ///
    /// 上一世已经 `start_call` 过的那次：没有副作用，不核对，把结果再落到那次尝试上就是了
    /// ——上一版计划仍从日志里读，这次调用自己还没有结果，不会算进去。
    async fn settle_plan(
        &self,
        request: &CallRequest,
        env: &CallEnv,
        plan: &ExecutionPlan,
        attempt: AttemptId,
    ) -> Result<(CallSettlement, bool), ExecError> {
        // `prepare` 校验过；这里还解不出来只可能是一份从账本读回的旧计划，照实报失败。
        let update = match serde_json::from_value::<PlanUpdate>(plan.args.clone()) {
            Ok(update) => update
                .validate()
                .map(|()| update)
                .map_err(|e| e.to_string()),
            Err(error) => Err(error.to_string()),
        };
        let update = match update {
            Ok(update) => update,
            Err(error) => {
                let content = format!("update_plan 的计划不合法：{error}");
                let settlement = self
                    .settle_orchestration(
                        request,
                        env,
                        attempt,
                        serde_json::json!({ "error": content }),
                        content,
                        true,
                    )
                    .await?;
                return Ok((settlement, false));
            }
        };

        let events = self.events_of(&env.session).await?;
        let previous = online_state(&events, &env.run).plan;
        let transition = analyze_transition(&previous, &update.steps);
        let boundary = !transition.completed.is_empty();
        let completed: Vec<&str> = transition
            .completed
            .iter()
            .map(|step| step.id.as_str())
            .collect();
        let result = serde_json::json!({
            "boundary": boundary,
            "completed_step_ids": completed,
            "progress_recorded": boundary && update.progress.is_some(),
            "plan": update.steps,
        });
        let content = std::iter::once(format_snapshot(&update.steps))
            .chain(transition.advice)
            .collect::<Vec<_>>()
            .join("\n");
        let settlement = self
            .settle_orchestration(request, env, attempt, result, content, false)
            .await?;
        Ok((settlement, boundary))
    }

    /// `dispatch` / `follow` / `update_plan` 共用的收尾：给那次尝试写一条结果，交回模型
    /// 一句话。
    async fn settle_orchestration(
        &self,
        request: &CallRequest,
        env: &CallEnv,
        attempt: AttemptId,
        result: serde_json::Value,
        content: String,
        is_error: bool,
    ) -> Result<CallSettlement, ExecError> {
        let body = ToolResultBody {
            status: if is_error {
                ToolResultStatus::Failed
            } else {
                ToolResultStatus::Completed
            },
            result,
            error: is_error.then(|| content.clone()),
            exit_code: None,
            artifacts: vec![],
            preview: Some(content),
        };
        let recorded = self
            .settle_attempt(
                AttemptRef {
                    session: env.session.clone(),
                    run: env.run.clone(),
                    call: request.call.clone(),
                    attempt,
                },
                body,
            )
            .await?;
        Ok(CallSettlement::Result(for_model(request, env, &recorded)))
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
            preview: Some(summary),
        };
        self.settle_attempt(attempt_ref, body).await?;
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
    ) -> Result<Option<Recorded>, ExecError> {
        let Some(previous) = previous else {
            return Ok(None);
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
            // 核对结论也是"工具结果"：事件里那 1 KiB 就取它。
            preview: Some(summary.to_string()),
        };
        self.settle_attempt(attempt_ref, body).await.map(Some)
    }

    /// 把一条结果落到**指定的那次尝试**上：发布输出 → 追加 `tool.result`。
    ///
    /// 三条路共用它，因为它们做的是同一件事——给某一世的尝试写下结论：工具自带核对函数的
    /// 结论（§8.6）、操作者在清单上按的键（§7.5）、子 Run 的终态回到父侧那次调用上（§4）。
    ///
    /// `elapsed_ms` 不是那次执行跑了多久（**我们不知道**），是输出存储从 `begin` 到
    /// `publish` 量到的那一段——账本里记的就是它，投影也只认它。
    async fn settle_attempt(
        &self,
        attempt_ref: AttemptRef,
        body: ToolResultBody,
    ) -> Result<Recorded, ExecError> {
        let writer = self.outputs.begin(&attempt_ref).await?;
        let recorded = self.publish(writer, body).await?;
        self.ledger
            .finish_call(&attempt_ref.attempt, recorded.published.clone())
            .await?;
        Ok(recorded)
    }

    /// 发布一份结果，留下投影要的那几格：工具正文与产物在 `output.json` 里，引用、大小与
    /// 耗时在账本那条 `tool.result` 里（[`Recorded`]）。
    async fn publish(
        &self,
        writer: Box<dyn OutputWriter>,
        body: ToolResultBody,
    ) -> Result<Recorded, ExecError> {
        let text = body.preview.clone();
        let artifacts = body.artifacts.clone();
        let published = self.outputs.publish(writer, body).await?;
        Ok(Recorded {
            published,
            text,
            artifacts,
        })
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
            preview: Some(message),
        };
        let writer = self.outputs.begin(&attempt_ref).await?;
        let recorded = self.publish(writer, body).await?;
        self.ledger
            .fail_call(
                &request.call,
                &attempt_ref.attempt,
                recorded.published.clone(),
            )
            .await?;
        Ok(CallSettlement::Result(for_model(request, env, &recorded)))
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
                    let (proof, used) = spent(approval, consumed);
                    return Ok(Authorization::Proceed {
                        grants: Some(used).into(),
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
                                let (proof, used) = spent(approval, consumed);
                                Ok(Authorization::Proceed {
                                    grants: Some(used).into(),
                                    proof,
                                })
                            }
                            // 授权在这一瞬间不再可用：不要硬凑一个 Proof，退回去问人。
                            _ => Ok(self.ask(request, plan, reason, vec![])),
                        }
                    }
                    // 组合计划两步各凭各的放行：逐步消费。
                    None if plan.then_run.is_some() => {
                        self.authorize_steps(request, plan, &decision_env, reason, intent)
                            .await
                    }
                    None => Ok(Authorization::Proceed {
                        proof: allowed(reason),
                        grants: GrantUses::default(),
                    }),
                }
            }
            PolicyDecision::Ask { reason, scopes } => Ok(self.ask(request, plan, reason, scopes)),
        }
    }

    /// 组合计划的两步**各凭各的**放行（Policy 已答 Allow，但没有哪一条授权盖住整份计划）。
    ///
    /// 每一步按**它自己**的单步计划（去掉 `then_run`）找盖住它的范围授权，并按那份计划
    /// 消费——与单步调用同一个 `Grant::covers`，消费那一刻再核一次有效期与撤销。没有授权
    /// 的那一步就是配置 Allow。任何一步的授权这一刻用不了，整份计划退回去问人（只给
    /// Once，与 Policy 对组合计划的 Ask 同一个口径）——没有半截放行。
    ///
    /// 用到的每一条授权按步记进 [`GrantUses`]，凭据也按步装（[`Proof::with_then_run`]）：
    /// 第二步换出来的凭据说得出它自己凭的是哪条。
    async fn authorize_steps(
        &self,
        request: &CallRequest,
        plan: &ExecutionPlan,
        decision_env: &DecisionEnv<'_>,
        reason: String,
        intent: ConsumeIntent,
    ) -> Result<Authorization, ExecError> {
        let mut settled: Vec<(Proof, Option<GrantUse>)> = Vec::with_capacity(2);
        for step in plan.steps() {
            let alone = ExecutionPlan {
                then_run: None,
                ..step.clone()
            };
            let Some(grant) = self.policy.step_grant(&alone, decision_env) else {
                settled.push((allowed(reason.clone()), None));
                continue;
            };
            match self
                .approvals
                .settle(&grant.approval, &alone, intent)
                .await?
            {
                ApprovalOutcome::Approved { approval, consumed } => {
                    let (proof, used) = spent(approval, consumed);
                    settled.push((proof, Some(used)));
                }
                _ => return Ok(self.ask(request, plan, reason, vec![])),
            }
        }
        let mut steps = settled.into_iter();
        let (Some((first, plan_grant)), Some((second, then_run_grant)), None) =
            (steps.next(), steps.next(), steps.next())
        else {
            unreachable!("组合计划正好两步（validate_steps 在 Policy 里判过）")
        };
        Ok(Authorization::Proceed {
            proof: first.with_then_run(second),
            grants: GrantUses {
                plan: plan_grant,
                then_run: then_run_grant,
            },
        })
    }

    /// 需要人看一眼：**只组装，不落盘**。
    ///
    /// 落盘在 [`Self::raise_ask`]——调用点先收完在飞的调用再落它（[`Self::execute_round`]）。
    /// 这一条区分不是洁癖：审批行比 Run 的挂起早出现一整个批次时，操作者可能答得比 Run
    /// 停下还早，那条答复就落进了空窗（§7.4）。
    fn ask(
        &self,
        request: &CallRequest,
        plan: &ExecutionPlan,
        reason: String,
        scopes: Vec<komo_kernel::types::chat::ApprovalScope>,
    ) -> Authorization {
        Authorization::Ask(Box::new(AskPending {
            call: request.call.clone(),
            plan: plan.clone(),
            reason,
            scopes,
        }))
    }

    /// 把一条待审批的请求落进 `approval_requests`：保存审批、暂停、等人（§7.4 的第一步）。
    async fn raise_ask(
        &self,
        pending: Box<AskPending>,
        env: &CallEnv,
    ) -> Result<ApprovalId, ExecError> {
        // 组合计划的 diff 只描述改动那一步：命令那一步界面另列"然后运行"（§11.3）。
        let changes = match self.tools.get(&pending.plan.tool) {
            Some(tool) => tool.changes(&pending.plan).await,
            None => None,
        };
        let record = self
            .approvals
            .request(ApprovalRequest {
                session: env.session.clone(),
                run: Some(env.run.clone()),
                call: Some(pending.call),
                plan: pending.plan,
                reason: pending.reason,
                changes,
                evidence: None,
                scopes: pending.scopes,
            })
            .await?;
        Ok(record.approval)
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
            mounts: env.mounts.clone(),
            env_version: env.env_version.clone(),
            resumed: request.resumed.clone(),
            cancel: env.cancel.clone(),
            call_timeout: env.call_timeout,
        }
    }
}

/// `prepare` / `verify` 时还没有尝试——`start_call` 才发号。用一个认得出来的哨兵，
/// 这样账本里的 attempt ID 与它不会混。
const PENDING_ATTEMPT: &str = "attempt-not-started";

/// 一个调用在**执行之前**的几种去向（[`ToolExecutor::begin`]）。
enum Begin {
    /// 放行过了，可以执行。只读的那一类允许被搬进在飞集合。
    ///
    /// 装箱不是洁癖：`Authorized` 里躺着一份完整的 `ExecutionPlan`（约 1.3 KB），而
    /// 这个枚举**每个调用都要过一手**——不装箱就是每一轮把那一坨来回搬。
    Ready(Box<Authorized>),
    /// 已经有结论：不需要执行（工具名不认识、参数准备不出来、被拒、核对收口），或者停在一条
    /// 已有的审批 / 交给人的判断上。
    Settled(CallSettlement),
    /// 需要人看一眼：**审批行还没有落**——`execute_round` 先收完在飞的再落它。
    Ask(Box<AskPending>),
    /// 放行过的编排调用（delegate / dispatch / follow / update_plan）：屏障，
    /// `execute_round` 先收完在飞的，才 `start_call` 并做它的事（[`Orchestrated`]）。
    Orchestrate(Box<Orchestrated>),
}

/// 一次放行过、还没开始做的编排调用。
struct Orchestrated {
    request: CallRequest,
    plan: ExecutionPlan,
    start: OrchestrationStart,
    kind: Orchestration,
}

/// 编排调用的四种。只看能力的那几道（深度、有没有接任务分发）在造它之前已经过了。
enum Orchestration {
    /// 受理一条子 Run，停在 `dependency` 上等它的终态。
    Delegate(DelegateSpec),
    /// 派一条后台任务，立刻收尾。
    Dispatch {
        spawner: Arc<dyn TaskSpawner>,
        spec: TaskSpec,
    },
    /// 往一条后台任务里转一句话，立刻收尾。
    Follow {
        spawner: Arc<dyn TaskSpawner>,
        task_id: String,
        text: String,
    },
    /// 记一版计划，和日志里的上一版比较。
    UpdatePlan,
}

enum OrchestrationStart {
    /// 首次：收尾时才 `start_call`，带着放行用掉的那条授权（若有）。
    Fresh(GrantUses),
    /// 上一世已经 `start_call` 过：不重新放行，结果落回那次尝试
    /// （`request.resumed.previous_attempt`）。
    Started,
}

/// 一条**已经拿到凭据**、可以直接执行的调用。
struct Authorized {
    request: CallRequest,
    tool: Arc<dyn Tool>,
    plan: ExecutionPlan,
    proof: Proof,
    grants: GrantUses,
    resumed: Option<ResumedCall>,
}

impl Authorized {
    /// 能不能和同一轮里后面的调用**同时在飞**（§6）。
    ///
    /// 两条都要：计划是只读的（`read` / `rg`），而且这一次没有"上一世"要接——**已经
    /// `start` 过**的调用要走核对梯子（§8.6），它的顺序由账本钉死，不能和谁抢跑道。
    /// 续跑里"确定尚未执行"的那条（`Planned`、一次尝试都没有）与新鲜调用等价，照样算。
    fn parallel_with_siblings(&self) -> bool {
        self.plan.operation.is_read_only()
            && self
                .resumed
                .as_ref()
                .is_none_or(ResumedCall::is_known_not_to_have_run)
    }
}

/// 一条还没落盘的审批请求。放行的最后一步才把它变成 `approval_requests` 里的一行。
struct AskPending {
    call: ToolCallId,
    plan: ExecutionPlan,
    reason: String,
    scopes: Vec<komo_kernel::types::chat::ApprovalScope>,
}

/// 一轮里已经进入执行、还没收尾的调用。**只读的才允许同时在飞**（§6）。
///
/// 装的是 `Send` 的 boxed future：`AgentLoop` 自己被要求 `Send`，装配它的那一段不能因为
/// 这里多了一路并发就变成不能跨线程。一次一轮，最多几条，这点装箱是它的代价。
type InFlight<'a> = FuturesUnordered<
    Pin<Box<dyn Future<Output = (usize, Result<CallSettlement, ExecError>)> + Send + 'a>>,
>;

enum CallSettlement {
    Result(ToolResultForModel),
    Stopped {
        stop: RoundStop,
        result: Option<ToolResultForModel>,
    },
}

/// 一次结果落盘之后，投影要的全部事实：账本那条 `tool.result` 的元信息，加上
/// `output.json` 里的工具正文与产物。回放从同样这几处读回来（§8.3）。
struct Recorded {
    published: PublishedOutput,
    text: Option<String>,
    artifacts: Vec<ContentRef>,
}

/// 交给模型的那一份：**每条结果都从这里过**，与回放（`komo-agent` 的
/// `to_replay_messages`）调同一个 [`project_views`] / [`select`]、喂同样的事实，所以刚跑完与重启之后回放
/// 逐字节相同（§8.3）。工具名取 `request.tool`——它就是落盘的那条 `tool_calls[].name`。
fn for_model(request: &CallRequest, env: &CallEnv, recorded: &Recorded) -> ToolResultForModel {
    let published = &recorded.published;
    let facts = ToolResultFacts {
        tool: &request.tool,
        status: published.status,
        elapsed_ms: published.elapsed_ms,
        text: recorded.text.as_deref(),
        output: &published.output,
        stdout: published.stdout.as_ref(),
        stderr: published.stderr.as_ref(),
        artifacts: &recorded.artifacts,
    };
    // 刚跑完的结果从不处于已衰减状态；之后要不要换短视图是 planner 对前缀的改写决策。
    let content = select(project_views(&facts, &env.projection), false);
    ToolResultForModel {
        provider_call_id: request.provider_call_id.clone(),
        call_id: request.call.clone(),
        content,
        is_error: published.status != ToolResultStatus::Completed,
    }
}

enum Authorization {
    Proceed {
        proof: Proof,
        grants: GrantUses,
    },
    Refused(String),
    /// 需要人看一眼，而且**这一条审批还没有落盘**（`AskPending`）。
    Ask(Box<AskPending>),
    /// 停在这条**已经存在**的审批上（`/approve` 唤醒后回来，决定还没写下来）。
    Waiting(ApprovalId),
}

/// 把在飞的每一路都收完：结果按**原始的调用序号**进 `settled`，返回它们当中第一条
/// 「停下来」的理由。
///
/// 收尾必须是全部：停在半路会让已经 `start_call` 过的那几条留在账本上，而下一世还会
/// 再来核对它们（§8.6）。所以这里不看 stop 直接往下收，stop 只记第一条。
async fn drain(
    in_flight: &mut InFlight<'_>,
    settled: &mut Vec<(usize, ToolResultForModel)>,
) -> Result<Option<RoundStop>, ExecError> {
    let mut stop = None;
    while let Some((index, outcome)) = in_flight.next().await {
        match outcome? {
            CallSettlement::Result(result) => settled.push((index, result)),
            CallSettlement::Stopped { stop: why, result } => {
                settled.extend(result.map(|result| (index, result)));
                stop.get_or_insert(why);
            }
        }
    }
    Ok(stop)
}

/// 收尾：结果按原始调用顺序交给模型，`remaining` 是还没轮到的那些（§6）。
///
/// 并发只改**执行**的顺序，不改交回模型的配对顺序：工具结果是按 `provider_call_id`
/// 认领的，但同一份事实每次都以同一个顺序排出来，重放与缓存才不会有第二种样子。
fn finish(
    stop: Option<RoundStop>,
    queue: VecDeque<(usize, CallRequest)>,
    mut settled: Vec<(usize, ToolResultForModel)>,
    plan_boundary: bool,
) -> RoundOutcome {
    settled.sort_by_key(|(index, _)| *index);
    RoundOutcome {
        results: settled.into_iter().map(|(_, result)| result).collect(),
        stop,
        remaining: queue.into_iter().map(|(_, request)| request).collect(),
        plan_boundary,
    }
}

/// 这一次尝试落盘的字节总数：`output.json` 加两个流。
///
/// 它是 §48 的 `tool_output_bytes`——"完整保存"这条承诺的量。
fn stored_bytes(published: &komo_kernel::types::refs::PublishedOutput) -> u64 {
    published.output.0.size
        + published.stdout.as_ref().map_or(0, |stream| stream.size)
        + published.stderr.as_ref().map_or(0, |stream| stream.size)
}

/// 这次读取读的是不是**我们自己落盘的观察**（§48 的 `observation_recall_count`）。
///
/// 判据是路径落在当前 Session 的 `tool-output/` 或 `artifacts/` 底下——也就是投影省掉
/// 正文之后，模型回头去 `read` 那一份的次数。它要是很高，说明投影太狠了（§27 的
/// "省略不等于丢失"就是这么被检验的）。
fn is_recall(plan: &ExecutionPlan, env: &CallEnv) -> bool {
    if !plan.operation.is_read_only() {
        return false;
    }
    let Some(root) = env.mounts.session_root.as_deref() else {
        return false;
    };
    plan.paths().any(|path| {
        ["tool-output", "artifacts"]
            .iter()
            .any(|dir| path.starts_with(root.join(dir)))
    })
}

/// 一条消费成功的审批换成凭据，外加账本要记的那条 [`GrantUse`]。
fn spent(
    approval: ApprovalId,
    consumed: komo_kernel::types::plan::ConsumedApproval,
) -> (Proof, GrantUse) {
    let proof = consumed.into_proof();
    let used = GrantUse {
        approval,
        grant: proof.grant_id().cloned(),
    };
    (proof, used)
}

/// 配置直接放行的凭据。
fn allowed(reason: String) -> Proof {
    PolicyDecision::allow(reason)
        .into_proof()
        .expect("Allow 换得出 Proof")
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

/// 工具守着 `call_timeout` 自己超时之后，还要终止进程组、回收（TERM 与 KILL 各等 5s）、
/// 写完输出。兜底计时器多给这么一段，否则两边同时到点，赢的总是执行器——一个本来
/// 说得清"超时被杀"的结果就成了"结果不明"。
const REAP_GRACE: Duration = Duration::from_secs(15);

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
            // 工具写给模型看的那段正文跟着结果一起落进 `output.json`：模型上下文的投影读
            // 这一份，而 JSONL 里的事件只留它的前 1 KiB。
            preview: output.preview.clone(),
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
            // 工具失败时模型必须看见那句话（"版本冲突：a.txt"）——投影读的就是这一份。
            preview: Some(error.to_string()),
        },
    }
}

/// 有没有别的子 Run 已经在续 `target` 这条线（§4 的"是不是末端"）：不论那条子 Run 现在
/// 是什么状态——线只往后接，不分叉，已经有人接了就不能再接第二次。
fn find_resumer(surface: &komo_kernel::fold::Surface, target: &RunId) -> Option<RunId> {
    surface.runs.values().find_map(|view| {
        let spec = view.delegate.as_ref()?;
        (spec.resumes.as_ref() == Some(target)).then(|| view.run.clone())
    })
}

/// 顺着 `resumes` 往后一路找到这条线现在的末端（拒绝续跑时要点名是哪一条）。
fn tail_of(surface: &komo_kernel::fold::Surface, start: &RunId) -> RunId {
    let mut current = start.clone();
    while let Some(next) = find_resumer(surface, &current) {
        current = next;
    }
    current
}

/// 状态给人看的那个词——校验失败的理由里要说清楚"现在是什么样"。
fn state_word(state: RunState) -> &'static str {
    match state {
        RunState::Accepted => "accepted",
        RunState::Queued => "queued",
        RunState::Running => "running",
        RunState::Waiting => "waiting",
        RunState::Completed => "completed",
        RunState::Failed => "failed",
        RunState::Cancelled => "cancelled",
        RunState::Abandoned => "abandoned",
    }
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
fn child_result(spec: &DelegateSpec, child: &RunId, end: &RunEnd) -> ToolResultBody {
    let (body, content) = match end {
        RunEnd::Completed { final_message, .. } => {
            let text = final_message.as_deref().unwrap_or_default();
            match &spec.contract {
                // 没有契约：子代理的最后一条回复就是结果，父侧只能自己读。
                None => {
                    let mut result = outcome_map(child, end);
                    result.insert("final_message".into(), serde_json::json!(text));
                    let content = format!("子 Run {child} 已完成。它的最后一条回复：\n{text}");
                    (completed_body(result, &content), content)
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
    };
    // completed / failed 都能续（§4：cancelled / abandoned 不能）；给模型一句能直接照着
    // 做的提示，而不是让它自己想起来这条 id 还能被 resume。
    match end {
        RunEnd::Completed { .. } | RunEnd::Failed { .. } => {
            let hint =
                format!("\n\n还要接着这件事，再调一次 delegate 并带上 resume: \"{child}\"。");
            let mut body = body;
            body.preview = Some(format!("{content}{hint}"));
            body
        }
        _ => body,
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
            (completed_body(result, &content), content)
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
                let content = format!(
                    "子 Run {child} 已完成，但结果不符合契约（permissive 放行，未按契约交付）：\
                     {report}。它的最后一条回复：\n{text}"
                );
                (completed_body(result, &content), content)
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

fn completed_body(
    result: serde_json::Map<String, serde_json::Value>,
    text: &str,
) -> ToolResultBody {
    ToolResultBody {
        status: ToolResultStatus::Completed,
        result: serde_json::Value::Object(result),
        error: None,
        exit_code: None,
        artifacts: vec![],
        // 父侧那条结果的正文：模型看见的就是它（`tool.result` 里留前 1 KiB）。
        preview: Some(text.to_string()),
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
            preview: Some(summary.clone()),
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
