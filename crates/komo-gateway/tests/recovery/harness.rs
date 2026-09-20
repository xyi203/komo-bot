//! 这一组测试要的那一台 Gateway：共用件在
//! `komo_gateway::service::test_support::harness`（真数据目录、真 `service::start`、
//! 脚本化模型），这里只留恢复特有的那几件——**故障账本装饰器**（在 §8.5 的步骤之间停
//! 下）、停机之后直接篡改磁盘的那几个动作，以及事件配对的断言助手。
//!
//! 两条注入路：
//!
//! - **(a) 在 §8.5 的步骤之间停下**：[`FaultLedger`] 包住装配出来的 `Arc<dyn Ledger>`，
//!   到了指定那一步就报错并**毒化**——此后这台 Gateway 的账本全部写入都失败，等价于
//!   进程已死。
//! - **(b) 停机之后直接篡改磁盘**：删掉一份 output.json、截断 JSONL 的尾部、写坏中间
//!   一行。

#![allow(dead_code, unused_imports)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use komo_kernel::events::{Event, EventPayload};
use komo_kernel::traits::{Ledger, LedgerError};
use komo_kernel::types::ids::{AttemptId, EventId, ExecutorId, RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::plan::ExecutionPlan;
use komo_kernel::types::refs::PublishedOutput;
use komo_kernel::types::status::{RunEnd, WaitReason};
use komo_kernel::types::turn::{AcceptInput, Accepted, AssistantRound, EventBatch, GrantUse};

pub use komo_gateway::service::test_support::harness::*;

// ---------------------------------------------------------------- 数据目录上的那几件事

/// 故障注入与磁盘篡改——共用的 [`Home`] 之外，恢复这一组还要这些。
pub trait Injected {
    /// 装一个故障账本；下一次 `start` 生效。
    fn inject(&self, fault: Fault) -> Arc<FaultState>;
    /// 撤掉注入（重启一台"好的"之前调它）。
    fn clear_injection(&self);
    /// 一次尝试的输出目录。
    fn attempt_dir(
        &self,
        session: &SessionId,
        run: &RunId,
        started: &komo_kernel::events::ToolStarted,
    ) -> PathBuf;
    /// 把一次尝试的输出整棵删掉（等价于 `publish` 从未发生）。
    fn drop_attempt_output(
        &self,
        session: &SessionId,
        run: &RunId,
        started: &komo_kernel::events::ToolStarted,
    );
    /// 在 `runtime/children/<executor>.json` 里记一个**活着**的子进程。
    fn register_live_child(&self, executor: &str, pid: u32);
}

impl Injected for Home {
    fn inject(&self, fault: Fault) -> Arc<FaultState> {
        let state = Arc::new(FaultState::new(fault));
        let for_wrap = Arc::clone(&state);
        komo_gateway::service::test_support::install_ledger_wrap(
            self.path(),
            Arc::new(move |inner| {
                Arc::new(FaultLedger {
                    inner,
                    state: Arc::clone(&for_wrap),
                }) as Arc<dyn Ledger>
            }),
        );
        state
    }

    fn clear_injection(&self) {
        komo_gateway::service::test_support::clear_ledger_wrap(self.path());
    }

    fn attempt_dir(
        &self,
        session: &SessionId,
        run: &RunId,
        started: &komo_kernel::events::ToolStarted,
    ) -> PathBuf {
        self.session_dir(session)
            .join("tool-output")
            .join(run.as_str())
            .join(started.call_id.as_str())
            .join(started.attempt_id.as_str())
    }

    /// 把这次尝试的输出整棵删掉——等价于**`ToolOutputStore::publish` 从未发生**。
    ///
    /// 故障装饰器包的是 `Ledger`，包不到 `ToolOutputStore`，而"外部副作用已经发生、
    /// 完整输出还没落盘"这一段（§14 故障注入表第 6 行）的分界正好在 `publish` 上。所以
    /// 这一刀走的是另一条注入路：停机之后直接改磁盘。`orphan::find` 对"文件不在"的判断
    /// 就是"没跑到落盘那一步"，与真正的中断无法区分。
    fn drop_attempt_output(
        &self,
        session: &SessionId,
        run: &RunId,
        started: &komo_kernel::events::ToolStarted,
    ) {
        let dir = self.attempt_dir(session, run, started);
        std::fs::remove_dir_all(&dir).unwrap_or_else(|e| panic!("删 {}：{e}", dir.display()));
    }

    /// 在 `runtime/children/<executor>.json` 里登记一个**还活着**的子进程
    /// （§8.7「无法确认旧执行已结束时，阻止该任务重复启动」）。
    fn register_live_child(&self, executor: &str, pid: u32) {
        let dir = self.path().join("runtime").join("children");
        std::fs::create_dir_all(&dir).expect("children 目录");
        let body = serde_json::json!([{
            "pid": pid,
            "pgid": pid,
            "what": "shell: 上一代留下的"
        }]);
        std::fs::write(
            dir.join(format!("{executor}.json")),
            serde_json::to_string_pretty(&body).unwrap(),
        )
        .expect("写子进程登记");
    }
}

// ---------------------------------------------------------------- 故障账本

/// 在哪一步停下。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// `start_run` 之前——Run 已经 queued，执行者一个字都没写下（表第 1 行）。
    BeforeStartRun,
    /// `start_call` 之前——计划已同步到 JSONL，`tool.started` 没有（表第 4 行）。
    BeforeStartCall,
    /// `start_call` 之后——started 已提交，真实动作还没发出（表第 5 行）。
    AfterStartCall,
    /// `finish_call` 之前——工具跑完、output.json 已发布，`tool.result` 没写（表第 6 / 8 行）。
    BeforeFinishCall,
    /// 第 n 次 `record_round` 之前（n 从 1 数）。用来在一轮工具收尾之后停住。
    BeforeRecordRound(u32),
    /// `append_audit` 之前——审批决定已在 state.db，审计事件补写不进 JSONL（表第 10 行）。
    BeforeAppendAudit,
    /// 不注入任何故障（只为了拿一个可观察的账本）。
    None,
}

pub struct FaultState {
    fault: Fault,
    rounds: AtomicUsize,
    tripped: tokio::sync::Notify,
    poisoned: std::sync::atomic::AtomicBool,
    trip_count: AtomicUsize,
}

impl FaultState {
    fn new(fault: Fault) -> FaultState {
        FaultState {
            fault,
            rounds: AtomicUsize::new(0),
            tripped: tokio::sync::Notify::new(),
            poisoned: std::sync::atomic::AtomicBool::new(false),
            trip_count: AtomicUsize::new(0),
        }
    }

    /// 等故障跳闸。跳闸之后这台 Gateway 的账本**全部写入都失败**，等价于进程已死。
    pub async fn wait_tripped(&self) {
        if self.poisoned.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::timeout(std::time::Duration::from_secs(20), self.tripped.notified())
            .await
            .expect("故障没有跳闸");
    }

    pub fn tripped(&self) -> bool {
        self.poisoned.load(Ordering::SeqCst)
    }

    fn trip(&self) -> LedgerError {
        self.poisoned.store(true, Ordering::SeqCst);
        self.trip_count.fetch_add(1, Ordering::SeqCst);
        self.tripped.notify_waiters();
        LedgerError::Persist("注入的故障：这个进程从这一步起写不动账本了".into())
    }

    /// 记一次穿过故障点，但**不毒化**：给那些不在 Run 推进路径上的写用（审计补写）。
    ///
    /// 毒化对推进路径上的故障点是必须的（见 [`FaultLedger`] 的注释），对补写却会把随后
    /// 每一次账本写入也一起废掉——那表达的是"进程死了"，由 `stop()` 表达就够了。补写要
    /// 表达的是"这一条审计没能落进 JSONL"，重启之后还得能再来一次。
    fn refuse(&self) -> LedgerError {
        self.trip_count.fetch_add(1, Ordering::SeqCst);
        self.tripped.notify_waiters();
        LedgerError::Persist("注入的故障：这一条审计补写不进去".into())
    }

    fn poisoned(&self) -> Option<LedgerError> {
        self.poisoned
            .load(Ordering::SeqCst)
            .then(|| LedgerError::Persist("注入的故障：这个进程从这一步起写不动账本了".into()))
    }
}

/// 包在真账本外面的故障装饰器。
///
/// 毒化是刻意的：一次 `Err` 会让调度器 `release` 之后**再领一次**，于是同一个故障点会
/// 被反复穿过，日志上多出几轮谁也没要求的事件。毒化之后所有写入都失败，Run 干净地停在
/// 原地，测试可以从容 `stop()`。
struct FaultLedger {
    inner: Arc<dyn Ledger>,
    state: Arc<FaultState>,
}

macro_rules! poisoned {
    ($self:expr) => {
        if let Some(error) = $self.state.poisoned() {
            return Err(error);
        }
    };
}

#[async_trait]
impl Ledger for FaultLedger {
    async fn accept_input(
        &self,
        input: AcceptInput,
    ) -> Result<komo_kernel::types::turn::Accepted, LedgerError> {
        poisoned!(self);
        self.inner.accept_input(input).await
    }

    async fn record_round(
        &self,
        run: &RunId,
        round: komo_kernel::types::turn::AssistantRound,
    ) -> Result<Vec<ToolCallId>, LedgerError> {
        poisoned!(self);
        let n = self.state.rounds.fetch_add(1, Ordering::SeqCst) as u32 + 1;
        if self.state.fault == Fault::BeforeRecordRound(n) {
            return Err(self.state.trip());
        }
        self.inner.record_round(run, round).await
    }

    async fn start_run(
        &self,
        run: &RunId,
        executor: &ExecutorId,
        generation: u64,
    ) -> Result<(), LedgerError> {
        poisoned!(self);
        if self.state.fault == Fault::BeforeStartRun {
            return Err(self.state.trip());
        }
        self.inner.start_run(run, executor, generation).await
    }

    async fn plan_call(
        &self,
        call: &ToolCallId,
        plan: &komo_kernel::types::plan::ExecutionPlan,
    ) -> Result<EventId, LedgerError> {
        poisoned!(self);
        self.inner.plan_call(call, plan).await
    }

    async fn start_call(
        &self,
        call: &ToolCallId,
        plan: &komo_kernel::types::plan::ExecutionPlan,
        grant: Option<komo_kernel::types::turn::GrantUse>,
    ) -> Result<AttemptId, LedgerError> {
        poisoned!(self);
        if self.state.fault == Fault::BeforeStartCall {
            return Err(self.state.trip());
        }
        let attempt = self.inner.start_call(call, plan, grant).await?;
        if self.state.fault == Fault::AfterStartCall {
            // JSONL 与 state.db 两步都提交了，**真实动作还没发出**。
            return Err(self.state.trip());
        }
        Ok(attempt)
    }

    async fn finish_call(
        &self,
        attempt: &AttemptId,
        published: komo_kernel::types::refs::PublishedOutput,
    ) -> Result<(), LedgerError> {
        poisoned!(self);
        if self.state.fault == Fault::BeforeFinishCall {
            return Err(self.state.trip());
        }
        self.inner.finish_call(attempt, published).await
    }

    async fn suspend(&self, run: &RunId, wait: WaitReason) -> Result<(), LedgerError> {
        poisoned!(self);
        self.inner.suspend(run, wait).await
    }

    async fn complete(
        &self,
        run: &RunId,
        end: komo_kernel::types::status::RunEnd,
    ) -> Result<(), LedgerError> {
        poisoned!(self);
        self.inner.complete(run, end).await
    }

    async fn read(
        &self,
        session: &SessionId,
        from: Seq,
        limit: u32,
    ) -> Result<komo_kernel::types::turn::EventBatch, LedgerError> {
        // 读永远放行：恢复扫描要读得到日志。
        self.inner.read(session, from, limit).await
    }

    async fn boundary(&self, session: &SessionId) -> Result<Seq, LedgerError> {
        poisoned!(self);
        self.inner.boundary(session).await
    }

    async fn append_audit(
        &self,
        session: &SessionId,
        event_id: &EventId,
        payload: EventPayload,
        occurred_at: time::OffsetDateTime,
    ) -> Result<Seq, LedgerError> {
        poisoned!(self);
        if self.state.fault == Fault::BeforeAppendAudit {
            // 补写**不在 Run 自己的推进路径上**（§8.5：权威已在 state.db，这一条是补写的
            // 审计副本），所以这里只拒绝这一次写入、不毒化整个账本——毒化会把 Run 的正常
            // 写入也一起废掉，"审计没写进去"就变成了"任务再也推不动"。
            return Err(self.state.refuse());
        }
        self.inner
            .append_audit(session, event_id, payload, occurred_at)
            .await
    }
}

// ---------------------------------------------------------------- 副作用计数

/// 一个副作用计数器：`shell` 往它里面追加一行，文件里有几行就是跑了几次。
pub struct Counter {
    path: PathBuf,
}

impl Counter {
    pub fn new(home: &Home, name: &str) -> Counter {
        Counter {
            path: home.workspace().join(name),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 让 `shell` 追加一行的命令。
    pub fn append_command(&self) -> String {
        format!("echo once >> {}", self.path.display())
    }

    /// 跑了几次。
    pub fn count(&self) -> usize {
        std::fs::read_to_string(&self.path)
            .map(|text| text.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------- 事件小工具

pub fn of_run<'a>(events: &'a [Event], run: &RunId) -> Vec<&'a Event> {
    events
        .iter()
        .filter(|event| event.run.as_ref() == Some(run))
        .collect()
}

pub fn tool_started(events: &[Event]) -> Vec<&komo_kernel::events::ToolStarted> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolStarted(body) => Some(body),
            _ => None,
        })
        .collect()
}

pub fn tool_results(events: &[Event]) -> Vec<&komo_kernel::events::ToolResult> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolResult(body) => Some(body),
            _ => None,
        })
        .collect()
}

/// 这个 Run 的结果事件的状态，按顺序。
pub fn result_statuses(events: &[Event]) -> Vec<komo_kernel::types::refs::ToolResultStatus> {
    tool_results(events)
        .iter()
        .map(|result| result.status)
        .collect()
}

pub fn planned_calls(events: &[Event]) -> Vec<ToolCallId> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolPlanned(body) => Some(body.call_id.clone()),
            _ => None,
        })
        .collect()
}

/// 模型这一轮要求的调用号（`message.assistant` 里的）。
pub fn requested_calls(events: &[Event]) -> Vec<ToolCallId> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::MessageAssistant(body) => Some(body.tool_calls.clone()),
            _ => None,
        })
        .flatten()
        .map(|call| call.call_id)
        .collect()
}

/// **事件配对**：每个 `tool.started` 有且只有一个 `tool.result`，或者一个明确的
/// uncertain / 中断说明。返回没有配上的那些 attempt。
pub fn unpaired_attempts(events: &[Event]) -> Vec<AttemptId> {
    let settled: Vec<AttemptId> = tool_results(events)
        .iter()
        .map(|result| result.attempt_id.clone())
        .collect();
    tool_started(events)
        .iter()
        .map(|started| started.attempt_id.clone())
        .filter(|attempt| !settled.contains(attempt))
        .collect()
}

pub fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
