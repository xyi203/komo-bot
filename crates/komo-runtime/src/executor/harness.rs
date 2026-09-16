//! executor / agent 测试共用的装配（只在 `cfg(test)` 下编译）。
//!
//! 替身全部来自 kernel 的 `test_support`：账本真的按 seq 追加、审批真的幂等、输出
//! 存储真的能把发布过的正文读回来。所以这些测试断言的是**实际发生了什么**，不是
//! "状态变成了 running"。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use komo_kernel::policy::RuleTable;
use komo_kernel::test_support::{
    MemApprovalRepo, MemLedger, MemOutputStore, TestClock, sample_model,
};
use komo_kernel::traits::{Clock, Ledger, Tool};
use komo_kernel::types::ids::{OperationId, RequestKey, RunId, SessionId, ToolCallId};
use komo_kernel::types::plan::{
    ApprovedPlan, ExecutionPlan, Operation, PlanSource, PlanVersions, RecoveryMode, Verification,
};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{
    CancelToken, ToolContext, ToolDefinition, ToolError, ToolOutput, WorkspaceRoot,
};
use komo_kernel::types::turn::{AcceptInput, AssistantRound, ToolCallRequest};

use crate::approvals::ApprovalGate;
use crate::executor::{CallEnv, CallRequest, ExecutionLimits, ToolExecutor};
use crate::policy::PolicyEngine;

pub struct Harness {
    pub clock: TestClock,
    pub ledger: Arc<MemLedger>,
    pub outputs: Arc<MemOutputStore>,
    pub approvals: MemApprovalRepo,
    pub gate: ApprovalGate,
    pub dir: tempfile::TempDir,
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

impl Harness {
    pub fn new() -> Self {
        let clock = TestClock::fixed();
        let approvals = MemApprovalRepo::new();
        let gate = ApprovalGate::new(Arc::new(approvals.clone()), Arc::new(clock.clone()));
        Self {
            ledger: Arc::new(MemLedger::new(clock.clone())),
            outputs: Arc::new(MemOutputStore::new()),
            approvals,
            gate,
            dir: tempfile::tempdir().expect("临时目录"),
            clock,
        }
    }

    pub fn executor(&self, tools: Vec<Arc<dyn Tool>>, policy: PolicyEngine) -> Arc<ToolExecutor> {
        Arc::new(
            ToolExecutor::new(
                tools,
                self.ledger.clone(),
                self.outputs.clone(),
                self.gate.clone(),
                policy,
                Arc::new(self.clock.clone()),
            )
            .with_limits(ExecutionLimits::default()),
        )
    }

    /// 什么都要问的最保守策略。
    pub fn conservative(&self, tools: Vec<Arc<dyn Tool>>) -> Arc<ToolExecutor> {
        self.executor(tools, PolicyEngine::conservative())
    }

    /// §7.1 那张初始建议表。
    pub fn initial(&self, tools: Vec<Arc<dyn Tool>>) -> Arc<ToolExecutor> {
        self.executor(tools, PolicyEngine::initial())
    }

    /// 全放行——测"执行本身"时不想被审批挡住。
    pub fn permissive(&self, tools: Vec<Arc<dyn Tool>>) -> Arc<ToolExecutor> {
        let mut table = RuleTable::empty();
        table.default = komo_kernel::policy::Effect::Allow;
        self.executor(tools, PolicyEngine::from_rules(table))
    }

    /// 开一个 Run，走真实的 `accept_input`。
    pub async fn open_run(&self) -> (SessionId, RunId) {
        let session = SessionId::from_raw("sess-1");
        let accepted = self
            .ledger
            .accept_input(AcceptInput {
                session: session.clone(),
                request_key: RequestKey::new("test:1"),
                text: "把 a.txt 写出来".into(),
                source: PlanSource::Interactive {
                    session: session.clone(),
                },
                peer: None,
                model: sample_model(),
                at: self.clock.now(),
            })
            .await
            .expect("接收输入");
        (session, accepted.run)
    }

    /// 记录一个回合（executor 的测试不经过模型，但调用必须先在账本里存在）。
    pub async fn record_round(
        &self,
        run: &RunId,
        calls: &[(&str, serde_json::Value)],
    ) -> Vec<CallRequest> {
        let requests: Vec<CallRequest> = calls
            .iter()
            .enumerate()
            .map(|(index, (name, args))| {
                CallRequest::fresh(
                    ToolCallId::from_raw(format!("call-{index}")),
                    format!("pc-{index}"),
                    *name,
                    args.clone(),
                )
            })
            .collect();
        let round = AssistantRound {
            round: 1,
            text: None,
            text_ref: None,
            tool_calls: requests
                .iter()
                .map(|request| ToolCallRequest {
                    call_id: request.call.clone(),
                    provider_call_id: request.provider_call_id.clone(),
                    name: request.tool.clone(),
                    arguments: request.arguments.clone(),
                    arguments_ref: None,
                })
                .collect(),
            provider_blocks: None,
            usage: Default::default(),
        };
        self.ledger
            .record_round(run, round)
            .await
            .expect("记录回合");
        requests
    }

    pub fn env(&self, session: &SessionId, run: &RunId) -> CallEnv {
        self.env_with_cancel(session, run, CancelToken::new())
    }

    pub fn env_with_cancel(
        &self,
        session: &SessionId,
        run: &RunId,
        cancel: CancelToken,
    ) -> CallEnv {
        let root = std::fs::canonicalize(self.dir.path()).expect("真实路径");
        CallEnv {
            session: session.clone(),
            run: run.clone(),
            source: PlanSource::Interactive {
                session: session.clone(),
            },
            cwd: root.clone(),
            roots: vec![WorkspaceRoot {
                path: root,
                writable: true,
                label: "workspace".into(),
            }],
            env_version: None,
            principal: None,
            cancel,
        }
    }
}

/// 一个记账的假工具：跑过几次、每次拿到的计划、`verify` 答什么，都可以断言和编排。
pub struct RecordingTool {
    name: &'static str,
    operation: Operation,
    recovery: RecoveryMode,
    verdict: std::sync::Mutex<Option<Verification>>,
    outcome: std::sync::Mutex<Option<Result<ToolOutput, ToolError>>>,
    pub executions: AtomicU32,
    pub verifications: AtomicU32,
}

impl RecordingTool {
    pub fn new(name: &'static str, operation: Operation) -> Self {
        Self {
            name,
            operation,
            recovery: RecoveryMode::NoSafeRecovery,
            verdict: std::sync::Mutex::new(None),
            outcome: std::sync::Mutex::new(None),
            executions: AtomicU32::new(0),
            verifications: AtomicU32::new(0),
        }
    }

    /// 一个"任意 shell 命令"形状的工具：初始规则表对它答 Ask。
    pub fn shell() -> Self {
        Self::new(
            "shell",
            Operation::ShellCommand {
                command: "rm -rf /tmp/x".into(),
            },
        )
    }

    pub fn with_recovery(mut self, recovery: RecoveryMode) -> Self {
        self.recovery = recovery;
        self
    }

    pub fn with_verdict(self, verdict: Verification) -> Self {
        *self.verdict.lock().expect("假工具") = Some(verdict);
        self
    }

    pub fn with_outcome(self, outcome: Result<ToolOutput, ToolError>) -> Self {
        *self.outcome.lock().expect("假工具") = Some(outcome);
        self
    }

    pub fn ran(&self) -> u32 {
        self.executions.load(Ordering::SeqCst)
    }

    pub fn verified(&self) -> u32 {
        self.verifications.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Tool for RecordingTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "测试用".into(),
            parameters: serde_json::json!({ "type": "object" }),
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        Ok(ExecutionPlan {
            operation_id: OperationId::from_raw(format!("op-{}", self.name)),
            source: ctx.source.clone(),
            tool: self.name.into(),
            operation: self.operation.clone(),
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args,
            cwd: Some(ctx.cwd.clone()),
            targets: vec![],
            versions: PlanVersions::default(),
            resources: vec![],
            recovery: self.recovery.clone(),
        })
    }

    async fn execute(
        &self,
        _plan: ApprovedPlan,
        _ctx: &ToolContext,
        _sink: &mut dyn komo_kernel::traits::OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.outcome
            .lock()
            .expect("假工具")
            .clone()
            .unwrap_or_else(|| {
                Ok(ToolOutput {
                    status: ToolResultStatus::Completed,
                    result: serde_json::json!({ "ok": true }),
                    exit_code: Some(0),
                    artifacts: vec![],
                    preview: Some("ok".into()),
                })
            })
    }

    async fn verify(
        &self,
        _plan: &ExecutionPlan,
        _ctx: &ToolContext,
    ) -> Result<Verification, ToolError> {
        self.verifications.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .verdict
            .lock()
            .expect("假工具")
            .clone()
            .unwrap_or(Verification::Unavailable))
    }
}
