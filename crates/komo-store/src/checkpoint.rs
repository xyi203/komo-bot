//! `checkpoints`：已覆盖的 seq、JSONL 字节位置、格式版本与记忆版本引用（§8.2、§8.3）。
//!
//! 检查点**不复制整段历史**：摘要正文是 JSONL 事件，这里只记范围（§8.3 最后一条实现
//! 约束）。字节位置只是加速索引——**校验不符时重新扫描并重建**，所以整张表是可重建表。
//!
//! resume 要重新核对被引用的记忆的当前状态与有效期：**过期检查点不能恢复已经遗忘的
//! 记忆**（§9.7）。这里只负责把 ID / revision 存下来，核对在 MemoryManager。

use komo_kernel::events::{Checkpoint, EventPayload};
use komo_kernel::traits::StoreError;
use komo_kernel::types::ids::{EventId, RunId, Seq, SessionId};
use komo_kernel::types::turn::{MemoryUse, SeqRange};
use time::OffsetDateTime;
use toasty::Executor;

use crate::db::{BoxFuture, Db, decode, encode, map_toasty, to_ts};
use crate::models::CheckpointRow;

/// 一个检查点。
#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointRecord {
    /// 承载这个检查点的事件 ID。
    pub event: EventId,
    pub session: SessionId,
    pub run: Option<RunId>,
    pub covers: SeqRange,
    /// JSONL 字节位置；`None` = 没记。
    pub byte_offset: Option<u64>,
    pub format_version: u32,
    pub memories: Vec<MemoryUse>,
    pub retrieval_config_version: Option<String>,
    /// 执行游标：这个检查点之后从哪条事件继续。
    pub cursor: Seq,
}

impl CheckpointRecord {
    /// 这个检查点作为 JSONL 事件的 payload。
    pub fn payload(&self) -> EventPayload {
        EventPayload::Checkpoint(Checkpoint {
            covers: self.covers,
            byte_offset: self.byte_offset,
            format_version: self.format_version,
            memories: self.memories.clone(),
            retrieval_config_version: self.retrieval_config_version.clone(),
        })
    }
}

/// 检查点的读写。
#[derive(Debug, Clone)]
pub struct CheckpointStore {
    db: Db,
}

impl CheckpointStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// 写一个检查点。按事件 ID 幂等。
    pub async fn put(&self, record: &CheckpointRecord) -> Result<(), StoreError> {
        let record = record.clone();
        let now = OffsetDateTime::now_utc();
        self.db
            .with_write_retry(move |ex| {
                let record = record.clone();
                Box::pin(async move { put_in(ex, &record, now).await })
                    as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
    }

    /// 一个 Session 最新的检查点。
    pub async fn latest(
        &self,
        session: &SessionId,
    ) -> Result<Option<CheckpointRecord>, StoreError> {
        let id = session.to_string();
        self.db
            .read(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    let mut rows =
                        CheckpointRow::filter(CheckpointRow::fields().session_id().eq(id.as_str()))
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?;
                    rows.sort_by_key(|row| row.covers_to);
                    match rows.last() {
                        Some(row) => Ok(Some(record_from_row(row)?)),
                        None => Ok(None),
                    }
                }) as BoxFuture<'_, Result<Option<CheckpointRecord>, StoreError>>
            })
            .await
    }

    /// 一个 Session 的全部检查点，按覆盖范围排序。
    pub async fn list(&self, session: &SessionId) -> Result<Vec<CheckpointRecord>, StoreError> {
        let id = session.to_string();
        self.db
            .read(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    let mut rows =
                        CheckpointRow::filter(CheckpointRow::fields().session_id().eq(id.as_str()))
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?;
                    rows.sort_by_key(|row| row.covers_to);
                    let mut out = Vec::with_capacity(rows.len());
                    for row in &rows {
                        out.push(record_from_row(row)?);
                    }
                    Ok(out)
                }) as BoxFuture<'_, Result<Vec<CheckpointRecord>, StoreError>>
            })
            .await
    }
}

/// 在一个已经打开的事务里写检查点——Coordinator 会把它和别的写放在一起。
pub async fn put_in(
    ex: &mut dyn Executor,
    record: &CheckpointRecord,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    if CheckpointRow::filter_by_id(record.event.as_str())
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?
        .is_some()
    {
        return Ok(());
    }
    toasty::create!(CheckpointRow {
        id: record.event.as_str(),
        session_id: record.session.as_str(),
        run_id: record.run.as_ref().map(|r| r.to_string()),
        covers_from: i64::try_from(record.covers.from.0).unwrap_or(i64::MAX),
        covers_to: i64::try_from(record.covers.to.0).unwrap_or(i64::MAX),
        byte_offset: record
            .byte_offset
            .map(|b| i64::try_from(b).unwrap_or(i64::MAX))
            .unwrap_or(0),
        format_version: i64::from(record.format_version),
        memories: encode(&record.memories)?,
        retrieval_config_version: record.retrieval_config_version.clone(),
        cursor: i64::try_from(record.cursor.0).unwrap_or(i64::MAX),
        created_at: to_ts(now),
    })
    .exec(ex)
    .await
    .map_err(map_toasty)?;
    Ok(())
}

fn record_from_row(row: &CheckpointRow) -> Result<CheckpointRecord, StoreError> {
    Ok(CheckpointRecord {
        event: EventId::from_raw(row.id.clone()),
        session: SessionId::from_raw(row.session_id.clone()),
        run: row.run_id.clone().map(RunId::from_raw),
        covers: SeqRange {
            from: Seq(row.covers_from.max(0) as u64),
            to: Seq(row.covers_to.max(0) as u64),
        },
        byte_offset: (row.byte_offset > 0).then_some(row.byte_offset as u64),
        format_version: row.format_version.max(0) as u32,
        memories: decode(&row.memories, "checkpoints.memories")?,
        retrieval_config_version: row.retrieval_config_version.clone(),
        cursor: Seq(row.cursor.max(0) as u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::types::turn::MemoryUse;

    async fn temp() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect(dir.path().join("state.db"))
            .await
            .expect("打开库");
        (db, dir)
    }

    fn record(event: &str, from: u64, to: u64) -> CheckpointRecord {
        CheckpointRecord {
            event: EventId::from_raw(event),
            session: SessionId::from_raw("sess-1"),
            run: Some(RunId::from_raw("run-1")),
            covers: SeqRange {
                from: Seq(from),
                to: Seq(to),
            },
            byte_offset: Some(4096),
            format_version: 1,
            memories: vec![MemoryUse {
                memory: komo_kernel::types::ids::MemoryId::from_raw("m-1"),
                revision: 3,
            }],
            retrieval_config_version: Some("retr-v1".into()),
            cursor: Seq(to),
        }
    }

    #[tokio::test]
    async fn a_checkpoint_round_trips_and_is_idempotent_by_event_id() {
        let (db, _dir) = temp().await;
        let store = CheckpointStore::new(db);
        store.put(&record("evt-1", 1, 41)).await.unwrap();
        store.put(&record("evt-1", 1, 41)).await.unwrap();

        let all = store.list(&SessionId::from_raw("sess-1")).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0], record("evt-1", 1, 41));
        // 记忆版本引用是审计证据：resume 要按它重新核对（§9.7）。
        assert_eq!(all[0].memories[0].revision, 3);
    }

    #[tokio::test]
    async fn the_latest_checkpoint_is_the_one_that_covers_furthest() {
        let (db, _dir) = temp().await;
        let store = CheckpointStore::new(db);
        store.put(&record("evt-1", 1, 41)).await.unwrap();
        store.put(&record("evt-2", 42, 88)).await.unwrap();

        let latest = store
            .latest(&SessionId::from_raw("sess-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.event.as_str(), "evt-2");
        assert_eq!(latest.covers.to, Seq(88));
        assert!(
            store
                .latest(&SessionId::from_raw("sess-2"))
                .await
                .unwrap()
                .is_none()
        );
    }

    /// 检查点作为 JSONL 事件时是 `checkpoint` 那一族——摘要正文不在这里（§8.3）。
    #[test]
    fn a_checkpoint_renders_as_its_event_payload() {
        let payload = record("evt-1", 1, 41).payload();
        assert_eq!(payload.type_name(), "checkpoint");
        let EventPayload::Checkpoint(body) = payload else {
            panic!()
        };
        assert_eq!(body.covers.to, Seq(41));
        assert_eq!(body.byte_offset, Some(4096));
        assert_eq!(body.memories.len(), 1);
    }

    /// 没记字节位置时读回来也是"没记"——`0` 不是一个真的偏移。
    #[tokio::test]
    async fn an_absent_byte_offset_reads_back_as_absent() {
        let (db, _dir) = temp().await;
        let store = CheckpointStore::new(db);
        let mut without = record("evt-1", 1, 41);
        without.byte_offset = None;
        store.put(&without).await.unwrap();
        let read = store
            .latest(&SessionId::from_raw("sess-1"))
            .await
            .unwrap()
            .unwrap();
        assert!(read.byte_offset.is_none());
    }
}
