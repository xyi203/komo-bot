//! 跨 crate 的测试替身（feature `test-support`，只作为 dev-dependency 启用，§13.4）。
//!
//! 这些替身要**够真**：store / runtime / gateway 的测试直接拿它们当依赖，所以
//! [`MemLedger`] 真的按 seq 追加事件、真的能 fold，[`MemApprovalRepo`] 真的幂等、真的
//! 在消费时核对计划哈希。一个只会返回 `Ok(())` 的替身测不出任何东西。

use std::sync::{Arc, Mutex};

use time::OffsetDateTime;

use crate::traits::Clock;
use crate::types::ids::{OperationId, SessionId};
use crate::types::plan::{ExecutionPlan, Proof, RecoveryMode};

mod approvals;
mod config;
mod cron;
mod ledger;
mod memory;
mod model;
mod output;
mod python;
mod queue;
#[cfg(test)]
mod tests;
mod zones;

pub use approvals::MemApprovalRepo;
pub use config::snapshot_fixture;
pub use cron::MemCronRepo;
pub use ledger::MemLedger;
pub use memory::MemMemoryRepo;
pub use model::{FixedEmbeddingClient, ScriptedLlm, ScriptedTurnDriver};
pub use output::{MemOutputStore, MemOutputWriter};
pub use python::FakePythonHost;
pub use queue::MemRunQueue;
pub use zones::ScriptedZoneResolver;

/// 测试用的 [`Proof`]。
///
/// 正式构建里 `Proof` 只有两个来源（`PolicyDecision::into_proof` 与
/// `ConsumedApproval::into_proof`），这个后门在 feature 门后面，不进生产（§4）。
pub fn proof() -> Proof {
    Proof::policy_allow()
}

/// 跑一个立即就绪的 future。
///
/// kernel 不依赖 tokio——**dev-dependency 也不加**，否则"kernel 不依赖 tokio"就成了一
/// 句只在 `cargo tree -e normal` 里成立的话。这些替身的 future 全部立即就绪，一个忙等
/// 的 `block_on` 就够跑它们。
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::hint::spin_loop(),
        }
    }
}

/// 可拨的时钟。
#[derive(Debug, Clone)]
pub struct TestClock {
    now: Arc<Mutex<OffsetDateTime>>,
}

impl TestClock {
    pub fn at(now: OffsetDateTime) -> Self {
        Self {
            now: Arc::new(Mutex::new(now)),
        }
    }

    /// 2026-09-15 08:00:00 UTC——设计文档里那几条示例事件的时间。
    pub fn fixed() -> Self {
        Self::at(time::macros::datetime!(2026-09-15 08:00:00 UTC))
    }

    pub fn advance(&self, by: time::Duration) {
        let mut now = self.now.lock().expect("测试时钟没有别的持有者会 panic");
        *now += by;
    }

    pub fn set(&self, to: OffsetDateTime) {
        *self.now.lock().expect("测试时钟") = to;
    }
}

impl Clock for TestClock {
    fn now(&self) -> OffsetDateTime {
        *self.now.lock().expect("测试时钟")
    }
}

/// 一份最小可用的执行计划，给不关心计划细节的测试用。
pub fn sample_plan(tool: &str, session: &SessionId) -> ExecutionPlan {
    ExecutionPlan {
        operation_id: OperationId::from_raw("op-test"),
        source: crate::types::plan::PlanSource::Interactive {
            session: session.clone(),
        },
        tool: tool.into(),
        operation: crate::types::plan::Operation::ReadFile,
        run: None,
        tool_call: None,
        args: serde_json::json!({}),
        cwd: None,
        targets: vec![],
        versions: Default::default(),
        resources: vec![],
        recovery: RecoveryMode::SafeReread,
    }
}

/// 一份最小可用的模型配置。
pub fn sample_model() -> crate::types::model::ModelConfig {
    crate::types::model::ModelConfig {
        provider: "test".into(),
        base_url: "memory://test".into(),
        model: "scripted".into(),
        api_key_env: "KOMO_TEST_KEY".into(),
        auth: None,
        effort: None,
        efforts: None,
        timeout_secs: 30,
    }
}
