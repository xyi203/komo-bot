//! `sessions` 与 `session_log_index`：store 内部的具体类型，只被 Coordinator 用（§13.5）。
//!
//! 这里的函数都接 `&mut dyn Executor`，所以调用方能把它们和别的写一起放进**同一个**
//! 事务——§8.5 的「state.db 事务写入事件引用、queued 与 applied_seq」说的就是一个事务。

use komo_kernel::traits::StoreError;
use komo_kernel::types::ids::{Seq, SessionId};
use std::collections::BTreeMap;
use time::OffsetDateTime;
use toasty::Executor;

use crate::db::{BoxFuture, Db, map_toasty, to_ts};
use crate::models::{SessionLogIndexRow, SessionRow};
use crate::session_log::AppendedEvent;

/// 一个 Session 的元数据，读出来的样子。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub session: SessionId,
    pub title: String,
    pub origin: String,
    pub workdir: Option<String>,
    pub current_run: Option<String>,
    pub jsonl_path: String,
    pub applied_seq: Seq,
    pub applied_bytes: u64,
}

impl From<&SessionRow> for SessionRecord {
    fn from(row: &SessionRow) -> Self {
        Self {
            session: SessionId::from_raw(row.id.clone()),
            title: row.title.clone(),
            origin: row.origin.clone(),
            workdir: row.workdir.clone(),
            current_run: row.current_run.clone(),
            jsonl_path: row.jsonl_path.clone(),
            applied_seq: Seq(row.applied_seq.max(0) as u64),
            applied_bytes: row.applied_bytes.max(0) as u64,
        }
    }
}

/// 读一个 Session 的元数据。
pub async fn get(db: &Db, session: &SessionId) -> Result<Option<SessionRecord>, StoreError> {
    let id = session.to_string();
    db.read(move |ex| {
        let id = id.clone();
        Box::pin(async move {
            let row = SessionRow::filter_by_id(&id)
                .first()
                .exec(ex)
                .await
                .map_err(map_toasty)?;
            Ok(row.as_ref().map(SessionRecord::from))
        }) as BoxFuture<'_, Result<Option<SessionRecord>, StoreError>>
    })
    .await
}

/// 在事务里读一个 Session。
pub async fn get_in(
    ex: &mut dyn Executor,
    session: &SessionId,
) -> Result<Option<SessionRow>, StoreError> {
    SessionRow::filter_by_id(session.as_str())
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 没有就建一行。**幂等**：已经在了就原样返回。
pub async fn ensure_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    origin: &str,
    jsonl_path: &str,
    now: OffsetDateTime,
) -> Result<SessionRow, StoreError> {
    if let Some(row) = get_in(ex, session).await? {
        return Ok(row);
    }
    toasty::create!(SessionRow {
        id: session.as_str(),
        title: String::new(),
        origin,
        workdir: None as Option<String>,
        current_run: None as Option<String>,
        jsonl_path,
        applied_seq: 0_i64,
        applied_bytes: 0_i64,
        created_at: to_ts(now),
        updated_at: to_ts(now),
    })
    .exec(ex)
    .await
    .map_err(map_toasty)
}

/// 推进 `applied_seq` / `applied_bytes`。
///
/// **只能推进连续、已校验的前缀**（§8.5），所以它只往前走：给一个比现在小的 seq 是
/// 调用方的错误，这里直接忽略而不是往回退——回退会让一段已经索引过的历史看起来又没
/// 索引过。
pub async fn advance_applied_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    seq: Seq,
    bytes: u64,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let Some(mut row) = get_in(ex, session).await? else {
        return Err(StoreError::NotFound {
            what: format!("session {session}"),
        });
    };
    let next = i64::try_from(seq.0).unwrap_or(i64::MAX);
    if next <= row.applied_seq {
        return Ok(());
    }
    row.update()
        .applied_seq(next)
        .applied_bytes(i64::try_from(bytes).unwrap_or(i64::MAX))
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 记一个 Session 的工作目录。
///
/// `ensure_in` 建行时永远写 `None`（它不知道），所以这条路是后补的那一次。**幂等且
/// 只在有值时写**：`None` 是"不改"，不是"清空"——一个已经绑好目录的会话不该因为下一
/// 条输入没带目录就回到 `workspaces/`。
pub async fn set_workdir_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    workdir: &str,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let Some(mut row) = get_in(ex, session).await? else {
        return Err(StoreError::NotFound {
            what: format!("session {session}"),
        });
    };
    if row.workdir.as_deref() == Some(workdir) {
        return Ok(());
    }
    row.update()
        .workdir(Some(workdir.to_string()))
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 记一个 Session 的当前 Run。
pub async fn set_current_run_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    run: Option<String>,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let Some(mut row) = get_in(ex, session).await? else {
        return Err(StoreError::NotFound {
            what: format!("session {session}"),
        });
    };
    row.update()
        .current_run(run)
        .updated_at(to_ts(now))
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// 把一条已经落盘的事件记进 `session_log_index`。
///
/// 按 `event_id` 幂等：重复提交同一 ID 不报错，也不改坐标（§8.3）。
pub async fn index_event_in(
    ex: &mut dyn Executor,
    appended: &AppendedEvent,
) -> Result<(), StoreError> {
    let id = appended.event.event_id.to_string();
    let existing = SessionLogIndexRow::filter_by_id(&id)
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?;
    if existing.is_some() {
        return Ok(());
    }
    toasty::create!(SessionLogIndexRow {
        id,
        session_id: appended.event.session.as_str(),
        seq: i64::try_from(appended.event.seq.0).unwrap_or(i64::MAX),
        run_id: appended.event.run.as_ref().map(|r| r.to_string()),
        event_type: appended.event.type_name(),
        byte_offset: i64::try_from(appended.byte_offset).unwrap_or(i64::MAX),
        byte_len: i64::try_from(appended.byte_len).unwrap_or(i64::MAX),
        digest: appended.digest.clone(),
    })
    .exec(ex)
    .await
    .map_err(map_toasty)?;
    Ok(())
}

/// 一个 Session 已提交记录的摘要，给 [`crate::session_log::TailExpectation`] 用。
pub async fn digests(db: &Db, session: &SessionId) -> Result<BTreeMap<Seq, String>, StoreError> {
    let id = session.to_string();
    db.read(move |ex| {
        let id = id.clone();
        Box::pin(async move {
            let rows = SessionLogIndexRow::filter(
                SessionLogIndexRow::fields().session_id().eq(id.as_str()),
            )
            .exec(ex)
            .await
            .map_err(map_toasty)?;
            Ok(rows
                .into_iter()
                .map(|row| (Seq(row.seq.max(0) as u64), row.digest))
                .collect::<BTreeMap<_, _>>())
        }) as BoxFuture<'_, Result<BTreeMap<Seq, String>, StoreError>>
    })
    .await
}

/// 全部 Session（按 id，也就是按创建时间——UUIDv7）。
pub async fn list(db: &Db) -> Result<Vec<SessionRecord>, StoreError> {
    db.read(move |ex| {
        Box::pin(async move {
            let rows = SessionRow::all().exec(ex).await.map_err(map_toasty)?;
            let mut out: Vec<SessionRecord> = rows.iter().map(SessionRecord::from).collect();
            out.sort_by(|a, b| a.session.as_str().cmp(b.session.as_str()));
            Ok(out)
        }) as BoxFuture<'_, Result<Vec<SessionRecord>, StoreError>>
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::events::{EVENT_FORMAT_VERSION, EventPayload, MessageUser};
    use komo_kernel::types::ids::EventId;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-15 08:00:00 UTC);

    async fn temp() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect(dir.path().join("state.db"))
            .await
            .expect("打开库");
        (db, dir)
    }

    fn appended(seq: u64, event: &str) -> AppendedEvent {
        AppendedEvent {
            event: komo_kernel::events::Event {
                v: EVENT_FORMAT_VERSION,
                seq: Seq(seq),
                event_id: EventId::from_raw(event),
                session: SessionId::from_raw("sess-1"),
                run: None,
                ts: NOW,
                payload: EventPayload::MessageUser(MessageUser {
                    text: Some("你好".into()),
                    text_ref: None,
                }),
            },
            byte_offset: seq * 100,
            byte_len: 100,
            digest: format!("{seq:064}"),
        }
    }

    async fn write<F>(db: &Db, op: F)
    where
        F: for<'a> Fn(&'a mut dyn Executor) -> BoxFuture<'a, Result<(), StoreError>>
            + Send
            + Sync
            + Clone
            + 'static,
    {
        db.with_write_retry(move |ex| op(ex)).await.unwrap();
    }

    #[tokio::test]
    async fn ensuring_a_session_twice_keeps_one_row() {
        let (db, _dir) = temp().await;
        write(&db, |ex| {
            Box::pin(async move {
                ensure_in(
                    ex,
                    &SessionId::from_raw("sess-1"),
                    "api",
                    "sessions/sess-1/events.jsonl",
                    NOW,
                )
                .await
                .map(|_| ())
            })
        })
        .await;
        write(&db, |ex| {
            Box::pin(async move {
                ensure_in(
                    ex,
                    &SessionId::from_raw("sess-1"),
                    "feishu",
                    "别的路径",
                    NOW,
                )
                .await
                .map(|_| ())
            })
        })
        .await;

        let all = list(&db).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].origin, "api", "已经在了就原样返回，不改写来源");
    }

    /// `applied_seq` 只能推进连续、已校验的前缀——**不往回退**（§8.5）。
    #[tokio::test]
    async fn applied_seq_only_moves_forward() {
        let (db, _dir) = temp().await;
        write(&db, |ex| {
            Box::pin(async move {
                ensure_in(ex, &SessionId::from_raw("sess-1"), "api", "p", NOW)
                    .await
                    .map(|_| ())
            })
        })
        .await;

        write(&db, |ex| {
            Box::pin(async move {
                advance_applied_in(ex, &SessionId::from_raw("sess-1"), Seq(5), 500, NOW).await
            })
        })
        .await;
        write(&db, |ex| {
            Box::pin(async move {
                advance_applied_in(ex, &SessionId::from_raw("sess-1"), Seq(2), 200, NOW).await
            })
        })
        .await;

        let record = get(&db, &SessionId::from_raw("sess-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.applied_seq, Seq(5));
        assert_eq!(record.applied_bytes, 500);
    }

    /// 索引按 `event_id` 幂等：重复提交同一 ID 不改坐标（§8.3）。
    #[tokio::test]
    async fn indexing_the_same_event_twice_keeps_the_first_coordinates() {
        let (db, _dir) = temp().await;
        write(&db, |ex| {
            Box::pin(async move { index_event_in(ex, &appended(1, "evt-1")).await })
        })
        .await;

        let mut moved = appended(1, "evt-1");
        moved.byte_offset = 999;
        moved.digest = "f".repeat(64);
        let moved_for_tx = moved.clone();
        db.with_write_retry(move |ex| {
            let moved = moved_for_tx.clone();
            Box::pin(async move { index_event_in(ex, &moved).await })
                as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();

        let digests = digests(&db, &SessionId::from_raw("sess-1")).await.unwrap();
        assert_eq!(digests.get(&Seq(1)).unwrap(), &format!("{:064}", 1));
    }

    #[tokio::test]
    async fn digests_are_keyed_by_seq() {
        let (db, _dir) = temp().await;
        for seq in 1..=3 {
            let event = appended(seq, &format!("evt-{seq}"));
            db.with_write_retry(move |ex| {
                let event = event.clone();
                Box::pin(async move { index_event_in(ex, &event).await })
                    as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
            .unwrap();
        }
        let digests = digests(&db, &SessionId::from_raw("sess-1")).await.unwrap();
        assert_eq!(digests.len(), 3);
        assert_eq!(digests[&Seq(2)], format!("{:064}", 2));
    }
}
