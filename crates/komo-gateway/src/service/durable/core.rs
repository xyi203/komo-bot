//! [`DurableCore`]：Durable 模式下 Gateway 手里的新存储与新 harness。
//!
//! 一个 `Engine`（提交线 + 任务调度）、一个 `Harness`（受理 submission、生成与工具任务的
//! 定义）、一个 `Storage`（读不经过线）。HTTP、渠道、cron 这些入口拿它：
//! `state.core()?`（或 `state.durable.as_ref()`）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use komo_kernel::durable::{ConversationId, Storage, SubmissionId, SubmissionRecord};
use komo_kernel::traits::Clock;
use komo_kernel::types::tool::InterjectSignal;
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
    /// 用户插话的旗子，按 conversation 一面（`bash` 转后台，`docs/monitor.md` §8）。
    interjections: Mutex<HashMap<ConversationId, InterjectSignal>>,
    /// faulted 任务的索引（W4a，`interventions`）：开库时接上提交订阅。
    pub(super) faults: Arc<super::interventions::FaultIndex>,
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
        let faults = Arc::new(super::interventions::FaultIndex::default());
        {
            // 订阅在引擎调度开始之前接上：之后出现的 faulted 任务一个不漏（Drop 不退订，随线一起清）。
            let faults = Arc::clone(&faults);
            let _ = engine
                .line()
                .subscribe_commits(move |publication| faults.observe(&publication.writes));
        }
        Ok(Arc::new(DurableCore {
            engine: Arc::new(engine),
            harness,
            storage,
            clock,
            home: home.to_path_buf(),
            closed: AtomicBool::new(false),
            interjections: Mutex::new(HashMap::new()),
            faults,
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

    /// 这个 conversation 的插话旗（没有就造一面）。工具环境里放的就是它。
    pub fn interjection(&self, conversation: ConversationId) -> InterjectSignal {
        self.interjections
            .lock()
            .expect("插话旗表")
            .entry(conversation)
            .or_default()
            .clone()
    }

    /// 放下旗子：新的一段（下一次模型请求）从没有插话开始——插话并进去以后自然落下，
    /// 没人看的旧旗不会让后面每条命令都提前转后台。
    pub fn lower_interjection(&self, conversation: ConversationId) {
        if let Some(signal) = self
            .interjections
            .lock()
            .expect("插话旗表")
            .get(&conversation)
        {
            signal.take();
        }
    }

    /// 用户往忙着的对话会话里插话了：前台的长 `bash` 提前转后台，别让这句话等它跑完。
    pub fn interject(&self, conversation: ConversationId) {
        self.interjection(conversation).raise();
    }

    /// 关闭：先让编排钩子终止自己起的进程（登记留给下一代对账），再封住受理、等已受理的提交结算与
    /// invocation 走完、关库（放掉 Turso 的文件锁）。幂等：第二次调用什么都不做。
    pub async fn close(&self) -> Result<(), DurableError> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.harness.deps().orchestration.shutdown().await;
        self.engine.close().await
    }
}
