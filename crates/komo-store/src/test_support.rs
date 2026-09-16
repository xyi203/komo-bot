//! 给上层 crate 的测试用的构造器（feature `test-support`，只作为 dev-dependency 启用）。
//!
//! runtime / gateway 的测试要一个**真的** Turso 库和一个真的 Session 目录——内存替身
//! （kernel 的 `MemLedger` / `MemOutputStore`）测得了 loop 与 executor 的逻辑，测不了
//! "写入顺序"和"尾部校验"。这里给的是后者。

use std::sync::Arc;

use komo_kernel::traits::{Clock, LedgerError, StoreError};
use komo_kernel::types::ids::SessionId;

use crate::coordinator::Coordinator;
use crate::db::{Db, DbOptions};
use crate::session_log::SessionPaths;

/// 一个临时数据目录 + 一个打开的 state.db。
///
/// `TempDir` 拿在手里：它一被丢弃，目录就没了，所以别让它比 [`Db`] 先走。
pub struct TempStore {
    root: tempfile::TempDir,
    db: Db,
}

impl std::fmt::Debug for TempStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TempStore")
            .field("root", &self.root.path())
            .finish_non_exhaustive()
    }
}

impl TempStore {
    /// 建一个临时数据目录，打开 `state.db`。
    pub async fn open() -> Result<TempStore, StoreError> {
        TempStore::open_with(DbOptions::default()).await
    }

    /// 同上，带选项——并发测试要调小池子或调退避时用。
    pub async fn open_with(options: DbOptions) -> Result<TempStore, StoreError> {
        let root =
            tempfile::tempdir().map_err(|e| StoreError::Io(format!("建立临时目录失败：{e}")))?;
        let db = Db::connect_with(root.path().join("state.db"), options).await?;
        Ok(TempStore { root, db })
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// 数据目录根（`~/.komo` 的替身）。
    pub fn root(&self) -> &std::path::Path {
        self.root.path()
    }

    /// `sessions/` 目录。
    pub fn sessions_root(&self) -> std::path::PathBuf {
        self.root.path().join("sessions")
    }

    pub fn paths_for(&self, session: &SessionId) -> SessionPaths {
        SessionPaths::new(self.sessions_root(), session)
    }

    /// 打开一个 Session 的 [`Coordinator`]。
    pub async fn coordinator(
        &self,
        session: &SessionId,
        clock: Arc<dyn Clock>,
    ) -> Result<Coordinator, LedgerError> {
        Coordinator::open(
            self.db.clone(),
            self.sessions_root(),
            session.clone(),
            "test",
            clock,
        )
        .await
    }

    /// 关掉这个库再用同一个目录重开一个——重启语义的测试要它。
    pub async fn reopen(self) -> Result<TempStore, StoreError> {
        let TempStore { root, db } = self;
        drop(db);
        let db = Db::connect(root.path().join("state.db")).await?;
        Ok(TempStore { root, db })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::test_support::TestClock;
    use komo_kernel::traits::Ledger;
    use komo_kernel::types::ids::{RequestKey, Seq};
    use komo_kernel::types::turn::AcceptInput;

    /// 上层 crate 拿到的就是这套：一个真的 Turso 库 + 一个真的 Session 目录，
    /// 关掉重开之后账本还在。
    #[tokio::test]
    async fn a_temp_store_survives_a_reopen() {
        let store = TempStore::open().await.expect("打开临时库");
        let session = SessionId::from_raw("00000000-0000-7000-8000-0000000000aa");
        let clock: Arc<dyn Clock> = Arc::new(TestClock::fixed());

        {
            let coordinator = store.coordinator(&session, clock.clone()).await.unwrap();
            coordinator
                .accept_input(AcceptInput {
                    session: session.clone(),
                    request_key: RequestKey::new("api:1"),
                    text: "你好".into(),
                    source: komo_kernel::types::plan::PlanSource::Interactive {
                        session: session.clone(),
                    },
                    peer: None,
                    model: komo_kernel::test_support::sample_model(),
                    at: clock.now(),
                })
                .await
                .unwrap();
        }

        assert!(store.paths_for(&session).events().exists());

        let store = store.reopen().await.expect("重开");
        let coordinator = store.coordinator(&session, clock).await.unwrap();
        let batch = coordinator.read(&session, Seq::ZERO, 0).await.unwrap();
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.next, None);
    }

    /// `Db::open_temp` 是最小的那一个：只要一个库。
    #[tokio::test]
    async fn open_temp_gives_a_usable_database() {
        let (db, dir) = Db::open_temp().await.expect("打开临时库");
        assert_eq!(
            db.table_ddl().await.unwrap().len(),
            crate::models::TABLES.len()
        );
        assert!(dir.path().join("state.db").exists());
    }
}
