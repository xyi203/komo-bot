//! [`DurableCore`]：Durable 模式下 Gateway 手里的新存储与新 harness。
//!
//! 一个 `Engine`（提交线 + 任务调度）、一个 `Harness`（受理 submission、生成与工具任务的
//! 定义）、一个 `Storage`（读不经过线）。HTTP、渠道、cron 这些入口拿它：
//! `state.core()?`（或 `state.durable.as_ref()`）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use komo_kernel::durable::{Storage, SubmissionId, SubmissionRecord};
use komo_kernel::traits::Clock;
use komo_runtime::codemode::Worker;
use komo_runtime::durable::harness::{AgentHooks, Harness, HarnessDeps, OrchestrationHooks};
use komo_runtime::durable::{DurableError, Engine, EngineOptions, Registry};
use komo_runtime::llm::ModelStream;
use komo_store::durable::TursoStorage;

pub struct DurableCore {
    pub engine: Arc<Engine>,
    pub harness: Harness,
    /// 已提交状态的读口（读不经过提交线）。
    pub storage: Arc<dyn Storage>,
    pub clock: Arc<dyn Clock>,
    /// 数据目录：`durable.db` 在这里，blob 在 `sessions/{uuid}/`。
    pub home: PathBuf,
    closed: AtomicBool,
}

impl std::fmt::Debug for DurableCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableCore")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl DurableCore {
    /// 打开 `<home>/durable.db` 并起引擎。**调度此刻是暂停的**：`GatewayState` 接好之后由
    /// `service::start` 调 [`Self::resume`]（钩子要先能找到 `GatewayState`，见 `assemble`）。
    pub async fn open(
        home: &Path,
        clock: Arc<dyn Clock>,
        stream: Arc<dyn ModelStream>,
        hooks: Arc<dyn AgentHooks>,
        orchestration: Arc<dyn OrchestrationHooks>,
        codemode: Option<Worker>,
    ) -> Result<Arc<DurableCore>, DurableError> {
        let storage: Arc<dyn Storage> = Arc::new(TursoStorage::open(home).await?);
        let deps = HarnessDeps::new(hooks, stream)
            .with_orchestration(orchestration)
            .with_codemode(codemode);
        let harness = Harness::new(deps);
        let registry = Registry::new();
        harness.install(&registry);
        let engine = Engine::open(
            Arc::clone(&storage),
            registry,
            Arc::clone(&clock),
            EngineOptions {
                hooks: harness.hooks(),
                ..Default::default()
            },
        )
        .await?;
        Ok(Arc::new(DurableCore {
            engine: Arc::new(engine),
            harness,
            storage,
            clock,
            home: home.to_path_buf(),
            closed: AtomicBool::new(false),
        }))
    }

    /// 开始（或继续）调度。幂等。
    pub fn resume(&self) {
        self.engine.resume();
    }

    /// 等一个 submission settle（`done` 或 `unanswered`）。
    pub async fn wait_submission(
        &self,
        id: SubmissionId,
    ) -> Result<SubmissionRecord, DurableError> {
        self.harness.wait(&self.engine, id).await
    }

    /// 关闭：封住受理、等已受理的提交结算与 invocation 走完、关库（放掉 Turso 的文件锁）。
    /// 幂等：第二次调用什么都不做。
    pub async fn close(&self) -> Result<(), DurableError> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.engine.close().await
    }
}
