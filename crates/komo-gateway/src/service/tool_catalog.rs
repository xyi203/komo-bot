//! 工具目录：Gateway 的 `ToolExecutor`（`tool_names` / `install` / `withdraw` /
//! `callable_definitions_for`，钩子与 MCP 挂载都读它）。
//!
//! 新存储下工具由 harness 的工具任务执行，这个执行器只当**目录**用：它的构造函数还要一个旧
//! `Ledger` 与 `ToolOutputStore`，这里给两个"没人会调用"的占位，调用就报错。构造函数与那两个
//! trait 在 runtime / kernel 里随旧 agent loop 一起删（D7b），到时候这个文件只剩一个函数。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::events::{ContextCompacted, EventPayload, MemoryInjected};
use komo_kernel::traits::{
    Clock, Ledger, LedgerError, OutputWriter, StoreError, Tool, ToolOutputStore,
};
use komo_kernel::types::ids::{AttemptId, EventId, ExecutorId, RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::plan::ExecutionPlan;
use komo_kernel::types::refs::{
    AttemptRef, InterruptedOutput, OutputRef, PublishedOutput, ToolResultBody, VerifiedOutput,
};
use komo_kernel::types::status::{RunEnd, WaitReason};
use komo_kernel::types::turn::{AcceptInput, Accepted, AssistantRound, EventBatch};
use komo_runtime::executor::ToolExecutor;
use time::OffsetDateTime;

/// 造目录执行器：全部内置工具 + codemode + MCP 连接。
pub(crate) fn catalog(
    tools: Vec<Arc<dyn Tool>>,
    codemode: Option<komo_runtime::codemode::Worker>,
    mcp: Arc<komo_runtime::mcp::McpHub>,
    clock: Arc<dyn Clock>,
) -> Arc<ToolExecutor> {
    Arc::new(
        ToolExecutor::new(tools, Arc::new(NoLedger), Arc::new(NoOutputs), clock)
            .with_codemode(codemode)
            .with_mcp(mcp),
    )
}

fn refuse() -> LedgerError {
    LedgerError::Conflict("旧账本已经不存在：工具由 harness 的任务执行".into())
}

struct NoLedger;

#[async_trait]
impl Ledger for NoLedger {
    async fn accept_input(&self, _input: AcceptInput) -> Result<Accepted, LedgerError> {
        Err(refuse())
    }
    async fn record_round(
        &self,
        _run: &RunId,
        _round: AssistantRound,
    ) -> Result<Vec<ToolCallId>, LedgerError> {
        Err(refuse())
    }
    async fn start_run(
        &self,
        _run: &RunId,
        _executor: &ExecutorId,
        _generation: u64,
    ) -> Result<(), LedgerError> {
        Err(refuse())
    }
    async fn plan_call(
        &self,
        _call: &ToolCallId,
        _plan: &ExecutionPlan,
    ) -> Result<EventId, LedgerError> {
        Err(refuse())
    }
    async fn start_call(
        &self,
        _call: &ToolCallId,
        _plan: &ExecutionPlan,
    ) -> Result<AttemptId, LedgerError> {
        Err(refuse())
    }
    async fn finish_call(
        &self,
        _attempt: &AttemptId,
        _published: PublishedOutput,
    ) -> Result<(), LedgerError> {
        Err(refuse())
    }
    async fn fail_call(
        &self,
        _call: &ToolCallId,
        _attempt: &AttemptId,
        _published: PublishedOutput,
    ) -> Result<(), LedgerError> {
        Err(refuse())
    }
    async fn suspend(&self, _run: &RunId, _wait: WaitReason) -> Result<(), LedgerError> {
        Err(refuse())
    }
    async fn complete(&self, _run: &RunId, _end: RunEnd) -> Result<(), LedgerError> {
        Err(refuse())
    }
    async fn read(
        &self,
        _session: &SessionId,
        _from: Seq,
        _limit: u32,
    ) -> Result<EventBatch, LedgerError> {
        Err(refuse())
    }
    async fn boundary(&self, _session: &SessionId) -> Result<Seq, LedgerError> {
        Err(refuse())
    }
    async fn record_compaction(
        &self,
        _run: &RunId,
        _compaction: ContextCompacted,
    ) -> Result<Seq, LedgerError> {
        Err(refuse())
    }
    async fn record_injection(
        &self,
        _run: &RunId,
        _injection: MemoryInjected,
    ) -> Result<Seq, LedgerError> {
        Err(refuse())
    }
    async fn append_audit(
        &self,
        _session: &SessionId,
        _event_id: &EventId,
        _payload: EventPayload,
        _occurred_at: OffsetDateTime,
    ) -> Result<Seq, LedgerError> {
        Err(refuse())
    }
    async fn run_end(&self, _run: &RunId) -> Result<Option<RunEnd>, LedgerError> {
        Err(refuse())
    }
}

struct NoOutputs;

fn no_store() -> StoreError {
    StoreError::Other("旧输出存储已经不存在：工具输出由 harness 的任务写进 Session 目录".into())
}

#[async_trait]
impl ToolOutputStore for NoOutputs {
    async fn begin(&self, _attempt: &AttemptRef) -> Result<Box<dyn OutputWriter>, StoreError> {
        Err(no_store())
    }
    async fn publish(
        &self,
        _writer: Box<dyn OutputWriter>,
        _result: ToolResultBody,
    ) -> Result<PublishedOutput, StoreError> {
        Err(no_store())
    }
    async fn open(&self, _output: &OutputRef) -> Result<VerifiedOutput, StoreError> {
        Err(no_store())
    }
    async fn interrupted(
        &self,
        _attempt: &AttemptRef,
        _tail_bytes: usize,
    ) -> Result<InterruptedOutput, StoreError> {
        Err(no_store())
    }
}
