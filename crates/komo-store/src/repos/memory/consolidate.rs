//! 离线整理的两处落库（§9.8）：读游标、一组一个事务地写。

use komo_kernel::traits::{ConsolidationWrite, RepoError, StoreError};
use komo_kernel::types::ids::MemoryId;
use komo_kernel::types::memory::MemoryState;
use time::OffsetDateTime;

use super::{clear_index_in, enum_str, put_in, require_revision};
use crate::db::{BoxFuture, Db, from_ts, map_toasty, to_ts};
use crate::models::MemoryItemRow;

pub(super) async fn marks(
    db: &Db,
    ids: &[MemoryId],
) -> Result<Vec<(MemoryId, Option<OffsetDateTime>)>, RepoError> {
    let ids: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
    db.read(move |ex| {
        let ids = ids.clone();
        Box::pin(async move {
            let mut out = Vec::with_capacity(ids.len());
            for id in &ids {
                let row = MemoryItemRow::filter_by_id(id)
                    .first()
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                out.push((
                    MemoryId::from_raw(id.clone()),
                    row.and_then(|row| row.consolidated_at).map(from_ts),
                ));
            }
            Ok(out)
        }) as BoxFuture<'_, Result<Vec<(MemoryId, Option<OffsetDateTime>)>, StoreError>>
    })
    .await
    .map_err(RepoError::from)
}

/// 先核对每个成员的 revision 与 `updated_at`（confirm / forget 不升 revision，所以要
/// 连 `updated_at` 一起看），再写，最后盖游标。游标只改 `consolidated_at` 一列。
pub(super) async fn apply(db: &Db, write: ConsolidationWrite) -> Result<(), RepoError> {
    db.with_write_retry(move |ex| {
        let write = write.clone();
        Box::pin(async move {
            for (id, revision, updated_at) in &write.expected {
                let row = require_revision(ex, id.as_str(), *revision).await?;
                if row.updated_at != to_ts(*updated_at) {
                    return Err(StoreError::VersionConflict {
                        expected: *revision,
                        actual: *revision,
                    });
                }
            }
            for item in write.puts {
                let revision = item.revision;
                put_in(ex, item, Some(revision)).await?;
            }
            for id in &write.forgets {
                let mut row = require_row(ex, id.as_str()).await?;
                row.update()
                    .state(enum_str(&MemoryState::Forgotten))
                    .updated_at(to_ts(write.stamp_at))
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                clear_index_in(ex, id.as_str()).await?;
            }
            for (id, _, _) in &write.expected {
                let mut row = require_row(ex, id.as_str()).await?;
                row.update()
                    .consolidated_at(Some(to_ts(write.stamp_at)))
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
            }
            Ok(())
        }) as BoxFuture<'_, Result<(), StoreError>>
    })
    .await
    .map_err(RepoError::from)
}

async fn require_row(ex: &mut dyn toasty::Executor, id: &str) -> Result<MemoryItemRow, StoreError> {
    MemoryItemRow::filter_by_id(id)
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?
        .ok_or_else(|| StoreError::NotFound {
            what: format!("memory {id}"),
        })
}
