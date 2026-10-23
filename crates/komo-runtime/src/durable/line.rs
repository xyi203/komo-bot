//! 提交线（`Line`，spec §4、§10.1；设计 §3.1）：一个 `Storage` 上串行执行提交闭包。
//!
//! 闭包在线上读表与 document、暂存写入；闭包结束后组成一批 `StorageWrite` 调
//! `Storage::commit`，成功后才发布给订阅者。存储的非"请求无效"错误让整条线失败：不再受理
//! 提交，由上层重开、从已提交状态恢复。**所有对 `Storage::commit` 的调用都在
//! [`Line::commit_to_storage`] 一处。**

use std::sync::{Arc, Mutex, PoisonError};

use futures_util::future::BoxFuture;
use tokio::sync::{Mutex as AsyncMutex, watch};

use komo_kernel::durable::{Seq, Storage, StorageWrite};
use komo_kernel::traits::Clock;

use super::error::DurableError;
use super::tx::{TaskScope, Tx};

/// 一次已提交批次的发布：订阅者在线上同步收到，此时存储里已经能读到它。
#[derive(Debug, Clone)]
pub struct Publication {
    pub seq: Seq,
    pub writes: Vec<StorageWrite>,
}

type CommitListener = Arc<dyn Fn(&Publication) + Send + Sync>;
type CloseListener = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct LineState {
    sealed: bool,
    failure: Option<DurableError>,
    next_listener: u64,
    commit_listeners: Vec<(u64, CommitListener)>,
    close_listeners: Vec<(u64, CloseListener)>,
}

struct Shared {
    storage: Arc<dyn Storage>,
    clock: Arc<dyn Clock>,
    /// 线本身：公平的 FIFO 互斥，持有者独占"闭包 + 装配 + 存储结算 + 发布"。
    gate: AsyncMutex<()>,
    state: Mutex<LineState>,
    sealed_tx: watch::Sender<bool>,
}

#[derive(Clone)]
pub struct Line {
    shared: Arc<Shared>,
}

/// 订阅的退订句柄；`Drop` 不退订（引擎关闭时整体清掉），显式 `dispose`。
pub struct Subscription {
    line: Line,
    id: u64,
}

impl Subscription {
    pub fn dispose(self) {
        let mut state = self.line.lock();
        state.commit_listeners.retain(|(id, _)| *id != self.id);
        state.close_listeners.retain(|(id, _)| *id != self.id);
    }
}

pub type TxFuture<'a, T> = BoxFuture<'a, Result<T, DurableError>>;

impl Line {
    pub fn new(storage: Arc<dyn Storage>, clock: Arc<dyn Clock>) -> Self {
        let (sealed_tx, _) = watch::channel(false);
        Self {
            shared: Arc::new(Shared {
                storage,
                clock,
                gate: AsyncMutex::new(()),
                state: Mutex::new(LineState::default()),
                sealed_tx,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LineState> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn storage(&self) -> &Arc<dyn Storage> {
        &self.shared.storage
    }

    /// 时钟，毫秒（unix）。
    pub fn now_ms(&self) -> i64 {
        (self.shared.clock.now().unix_timestamp_nanos() / 1_000_000) as i64
    }

    /// 线已关闭或失败时的错误。
    pub fn closed_error(&self) -> DurableError {
        match &self.lock().failure {
            Some(failure) => DurableError::Failed(failure.to_string()),
            None => DurableError::Closed,
        }
    }

    pub fn is_sealed(&self) -> bool {
        self.lock().sealed
    }

    pub fn failure(&self) -> Option<DurableError> {
        self.lock().failure.clone()
    }

    /// 线封住（关闭开始或失败）时解决。
    pub async fn sealed(&self) {
        let mut rx = self.shared.sealed_tx.subscribe();
        let _ = rx.wait_for(|sealed| *sealed).await;
    }

    pub fn subscribe_commits(
        &self,
        listener: impl Fn(&Publication) + Send + Sync + 'static,
    ) -> Subscription {
        let mut state = self.lock();
        state.next_listener += 1;
        let id = state.next_listener;
        state.commit_listeners.push((id, Arc::new(listener)));
        Subscription {
            line: self.clone(),
            id,
        }
    }

    /// 关闭监听：封口时同步调用一次（失败导致的关闭也调）；监听器不许阻塞或调线的 API。
    pub fn subscribe_close(&self, listener: impl Fn() + Send + Sync + 'static) -> Subscription {
        let mut state = self.lock();
        state.next_listener += 1;
        let id = state.next_listener;
        state.close_listeners.push((id, Arc::new(listener)));
        Subscription {
            line: self.clone(),
            id,
        }
    }

    /// 封口：不再受理提交与 reservation。幂等。
    pub fn seal(&self) {
        let listeners = {
            let mut state = self.lock();
            if state.sealed {
                return;
            }
            state.sealed = true;
            std::mem::take(&mut state.close_listeners)
        };
        for (_, listener) in listeners {
            listener();
        }
        self.shared.sealed_tx.send_replace(true);
    }

    /// 让线失败（存储错误或调度器自己的提交出错）：记下原因、封口。已封口则只记第一个。
    pub fn fail(&self, error: DurableError) {
        tracing::error!(error = %error, "durable line failed");
        {
            let mut state = self.lock();
            if state.failure.is_none() {
                state.failure = Some(error);
            }
        }
        self.seal();
    }

    /// 等已受理的提交结算完；`close_storage` 时再关存储。
    pub async fn finish_close(&self, close_storage: bool) -> Result<(), DurableError> {
        self.seal();
        let _gate = self.shared.gate.lock().await;
        if close_storage {
            self.shared.storage.close().await?;
        }
        Ok(())
    }

    fn admit(&self) -> Result<(), DurableError> {
        let state = self.lock();
        if !state.sealed {
            return Ok(());
        }
        Err(match &state.failure {
            Some(failure) => DurableError::Failed(failure.to_string()),
            None => DurableError::Closed,
        })
    }

    fn settle_error(&self, error: DurableError) -> DurableError {
        if error.is_fatal_storage() {
            self.fail(error.clone());
        }
        error
    }

    /// 在线上读（不产生写入）。已封口时仍可读（封口前排队的读照常完成），线失败则拒绝。
    pub async fn read_on_line<T>(
        &self,
        f: impl FnOnce() -> BoxFuture<'static, Result<T, DurableError>>,
    ) -> Result<T, DurableError> {
        let _gate = self.shared.gate.lock().await;
        if let Some(failure) = self.failure() {
            return Err(DurableError::Failed(failure.to_string()));
        }
        f().await.map_err(|error| self.settle_error(error))
    }

    /// 在线上执行一次提交：闭包读 / 暂存，结束后原子写入并发布。
    ///
    /// 排队在封口之前的提交，轮到时若已封口则被拒；轮到并开始的提交不受关闭影响，一定
    /// 结算（调用者取消也不撤销它）。闭包返回 `Err` 时整笔不生效。
    pub async fn commit<T: Send>(
        &self,
        scope: Option<TaskScope>,
        f: impl for<'a> FnOnce(&'a mut Tx) -> TxFuture<'a, T> + Send,
    ) -> Result<T, DurableError> {
        let _gate = self.shared.gate.lock().await;
        self.admit()?;
        let mut tx = Tx::new(self.shared.storage.clone(), self.now_ms(), scope);
        let value = match f(&mut tx).await {
            Ok(value) => value,
            Err(error) => return Err(self.settle_error(error)),
        };
        let writes = match tx.assemble().await {
            Ok(writes) => writes,
            Err(error) => return Err(self.settle_error(error)),
        };
        if writes.is_empty() {
            return Ok(value);
        }
        match self.commit_to_storage(&writes).await {
            Ok(seq) => {
                self.publish(Publication { seq, writes });
                Ok(value)
            }
            Err(error) => {
                let error = DurableError::from(error);
                Err(self.settle_error(error))
            }
        }
    }

    /// 唯一调 `Storage::commit` / `Storage::commit_progress` 的地方。
    ///
    /// 进度类提交——只有 `DocumentChange`（整值替换 document，§3.6）——走
    /// `commit_progress`，后端可以用更低的落盘档位；任何一条别的写入（entry、任务、
    /// submission、document 的建与退役）都让整批是结构性提交，走 `commit`。
    async fn commit_to_storage(
        &self,
        writes: &[StorageWrite],
    ) -> Result<Seq, komo_kernel::durable::StorageError> {
        if !writes.is_empty()
            && writes
                .iter()
                .all(|write| matches!(write, StorageWrite::DocumentChange { .. }))
        {
            return self.shared.storage.commit_progress(writes).await;
        }
        self.shared.storage.commit(writes).await
    }

    fn publish(&self, publication: Publication) {
        let listeners: Vec<CommitListener> = self
            .lock()
            .commit_listeners
            .iter()
            .map(|(_, listener)| listener.clone())
            .collect();
        for listener in listeners {
            listener(&publication);
        }
    }
}
