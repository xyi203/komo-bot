//! 任务定义（spec §5.1 的 `TaskDefinition`）与注册表。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use serde_json::Value;

use komo_kernel::durable::TaskRecord;
use komo_kernel::durable::records::TaskOutcome;
use komo_kernel::durable::task::{DefinitionInfo, RunningTask};

use super::error::{DurableError, TaskError};
use super::runtime::TaskRuntime;
use super::tx::Tx;

/// `migrate` 的结果：新版本的 input 与 checkpoint。
#[derive(Debug, Clone, PartialEq)]
pub struct Migrated {
    pub input: Value,
    pub checkpoint: Value,
}

/// 一种任务。checkpoint 是带字符串 `phase` 的 JSON 对象；每个 phase 之间的状态只在
/// checkpoint 里。
#[async_trait]
pub trait TaskKind: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn version(&self) -> u32 {
        1
    }
    /// 新任务的起始 checkpoint。
    fn initial(&self, input: &Value) -> Value;

    /// 跑 `task.phase()` 对应的阶段。一个阶段可以做几次提交围着一个外部效果（spec §5.2）；
    /// 返回前必须让 checkpoint 前进、进入等待或写下结局，否则按规则 6 `faulted`。
    /// 未知 phase 返回错误即可。
    async fn run_phase(&self, task: &RunningTask, rt: &TaskRuntime) -> Result<(), TaskError>;

    /// abort handler：补偿并提交 `terminal` 结局（通常 `aborted`）；不给结局就 `faulted`。
    async fn abort(&self, task: &RunningTask, rt: &TaskRuntime) -> Result<(), TaskError>;

    /// 迁移旧版本记录。`None` 表示这个定义没有迁移回调。一个回调处理所有受支持的旧版本。
    fn migrate(
        &self,
        _input: &Value,
        _checkpoint: &Value,
        _from_version: u32,
    ) -> Option<Result<Migrated, TaskError>> {
        None
    }

    fn has_migrate(&self) -> bool {
        false
    }

    fn info(&self) -> DefinitionInfo {
        DefinitionInfo {
            version: self.version(),
            has_migrate: self.has_migrate(),
        }
    }
}

/// 注册表某一刻的样子；一个阶段内保持不变。
#[derive(Clone, Default)]
pub struct RegistrySnapshot {
    kinds: Arc<HashMap<String, Arc<dyn TaskKind>>>,
}

impl RegistrySnapshot {
    pub fn task(&self, name: &str) -> Option<Arc<dyn TaskKind>> {
        self.kinds.get(name).cloned()
    }
}

/// 定义对象的身份（换定义 = 换对象，哪怕同版本）。
pub(super) fn identity(kind: &Arc<dyn TaskKind>) -> usize {
    Arc::as_ptr(kind) as *const () as usize
}

#[derive(Default)]
struct RegistryState {
    kinds: Arc<HashMap<String, Arc<dyn TaskKind>>>,
    next_listener: u64,
    listeners: Vec<(u64, Arc<dyn Fn() + Send + Sync>)>,
}

#[derive(Clone, Default)]
pub struct Registry {
    state: Arc<Mutex<RegistryState>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 装入（或换掉同名的）定义，然后通知订阅者重新考虑被阻塞的任务。
    pub fn install(&self, kind: Arc<dyn TaskKind>) {
        let listeners = {
            let mut state = self.lock();
            let mut kinds = (*state.kinds).clone();
            kinds.insert(kind.name().to_owned(), kind);
            state.kinds = Arc::new(kinds);
            state
                .listeners
                .iter()
                .map(|(_, l)| l.clone())
                .collect::<Vec<_>>()
        };
        for listener in listeners {
            listener();
        }
    }

    pub fn remove(&self, name: &str) {
        let listeners = {
            let mut state = self.lock();
            let mut kinds = (*state.kinds).clone();
            kinds.remove(name);
            state.kinds = Arc::new(kinds);
            state
                .listeners
                .iter()
                .map(|(_, l)| l.clone())
                .collect::<Vec<_>>()
        };
        for listener in listeners {
            listener();
        }
    }

    pub fn snapshot(&self) -> RegistrySnapshot {
        RegistrySnapshot {
            kinds: self.lock().kinds.clone(),
        }
    }

    pub(super) fn subscribe(&self, listener: impl Fn() + Send + Sync + 'static) -> u64 {
        let mut state = self.lock();
        state.next_listener += 1;
        let id = state.next_listener;
        state.listeners.push((id, Arc::new(listener)));
        id
    }

    pub(super) fn unsubscribe(&self, id: u64) {
        self.lock().listeners.retain(|(i, _)| *i != id);
    }
}

/// 非致命失败的出口（spec 的 `onReport`）。
pub type Reporter = Arc<dyn Fn(&str) + Send + Sync>;

/// 宿主给调度器的钩子：目前只有一个——调度器自己写的结局（`faulted` / `orphaned`）变成
/// terminal 的那笔提交里要做的清理（spec §5.4）。调度器不知道 run 是什么。
#[async_trait]
pub trait TaskHooks: Send + Sync {
    async fn settle_outcome(
        &self,
        _tx: &mut Tx,
        _record: &TaskRecord,
        _outcome: &TaskOutcome,
    ) -> Result<(), DurableError> {
        Ok(())
    }
}

pub struct NoHooks;

#[async_trait]
impl TaskHooks for NoHooks {}
