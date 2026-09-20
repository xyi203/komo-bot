//! `sessions` 与 `session_log_index`：store 内部的具体类型，只被 Coordinator 用（§13.5）。
//!
//! 这里的函数都接 `&mut dyn Executor`，所以调用方能把它们和别的写一起放进**同一个**
//! 事务——§8.5 的「state.db 事务写入事件引用、queued 与 applied_seq」说的就是一个事务。

use komo_kernel::traits::StoreError;
use komo_kernel::types::ids::{Seq, SessionId};
use komo_kernel::types::status::SessionState;
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
    /// 生命周期状态（§8.10）。
    pub state: SessionState,
    /// 状态变更时刻（写库时间；`0` 读成 Unix 纪元）。
    pub state_changed_at: OffsetDateTime,
}

impl SessionRecord {
    /// 从行读出来。**`state` 认不出就是损坏**（§8.10 第 1 条）：默认成 `active` 会把
    /// `deleted` / `purged` 的墓碑读成活的。
    fn try_from_row(row: &SessionRow) -> Result<SessionRecord, StoreError> {
        Ok(SessionRecord {
            session: SessionId::from_raw(row.id.clone()),
            title: row.title.clone(),
            origin: row.origin.clone(),
            workdir: row.workdir.clone(),
            current_run: row.current_run.clone(),
            jsonl_path: row.jsonl_path.clone(),
            applied_seq: Seq(row.applied_seq.max(0) as u64),
            applied_bytes: row.applied_bytes.max(0) as u64,
            state: state_of_row(row)?,
            state_changed_at: crate::db::from_ts(row.state_changed_at),
        })
    }

    /// 这个会话接不接受新输入（§8.10）。
    pub fn accepts_input(&self) -> bool {
        self.state.accepts_input()
    }
}

/// 一行里的生命周期状态。**认不出的值报错**，不挑默认值。
pub fn state_of_row(row: &SessionRow) -> Result<SessionState, StoreError> {
    SessionState::parse(&row.state).ok_or_else(|| {
        StoreError::Corrupt(format!(
            "sessions.state 认不出的值 {:?}（会话 {}）：只认 active / closing / deleted / purged",
            row.state, row.id
        ))
    })
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
            match row {
                Some(row) => Ok(Some(SessionRecord::try_from_row(&row)?)),
                None => Ok(None),
            }
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
        // 新会话一律 `active`；状态变更时刻与建行时刻一致（§8.10 第 1 条）。
        state: SessionState::Active.as_str(),
        state_changed_at: to_ts(now),
    })
    .exec(ex)
    .await
    .map_err(map_toasty)
}

/// 在事务里读一个 Session 的生命周期状态。行不在 = `None`。
pub async fn state_in(
    ex: &mut dyn Executor,
    session: &SessionId,
) -> Result<Option<SessionState>, StoreError> {
    let Some(row) = get_in(ex, session).await? else {
        return Ok(None);
    };
    state_of_row(&row).map(Some)
}

/// 读一个 Session 的生命周期状态。行不在 = `None`；值认不出 = 错误（§8.10）。
pub async fn state(db: &Db, session: &SessionId) -> Result<Option<SessionState>, StoreError> {
    let id = session.to_string();
    db.read(move |ex| {
        let id = id.clone();
        Box::pin(async move {
            let Some(row) = SessionRow::filter_by_id(&id)
                .first()
                .exec(ex)
                .await
                .map_err(map_toasty)?
            else {
                return Ok(None);
            };
            state_of_row(&row).map(Some)
        }) as BoxFuture<'_, Result<Option<SessionState>, StoreError>>
    })
    .await
}

/// **条件**推进生命周期状态：只有当前状态**恰好是** `from` 才写成 `to`。
///
/// 返回 `true` = 这次调用推进了它（`rows affected == 1`），`false` = 状态已经不是 `from`
/// 了，什么都没改。`from` 是 CAS：重跑一次不会把 `deleted` 盖回 `closing`——`komo
/// session delete`（`active → closing`）与 `purge`（`deleted → purged`）都可能被重试或
/// 并发调用，只有受影响行数能分辨"我推进了"与"别人已经推过了"（§8.10）。
///
/// **`purged` 是墓碑，没有出口**：内容已经删了，把它推回任何一个前面的状态都只会得到一个
/// 说不清自己内容的会话，所以那一类请求一律 `false`。§8.10 的"`purged` 之后没有任何路径再
/// 创建那个目录"要从这里就开始兜住，不能只靠调用方自觉。
///
/// **这是 raw SQL 的第四处**，理由与 §8.2 表里那句一样：toasty 的类型化 `UPDATE` 拿不到
/// 受影响行数，`rows affected` 是这里唯一可用的信号。
pub async fn set_state_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    from: SessionState,
    to: SessionState,
    now: OffsetDateTime,
) -> Result<bool, StoreError> {
    if from == SessionState::Purged {
        return Ok(false);
    }
    let at = to_ts(now);
    let affected = toasty::sql::statement(
        r#"UPDATE sessions
              SET state = ?1, state_changed_at = ?2, updated_at = ?2
            WHERE id = ?3 AND state = ?4"#,
    )
    .bind(to.as_str())
    .bind(at)
    .bind(session.as_str())
    .bind(from.as_str())
    .exec(ex)
    .await
    .map_err(map_toasty)?;
    Ok(affected == 1)
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

/// 会话的标题**只写一次**：空着才写（§12 的 `sessions.title`，`komo session list` 那一列）。
///
/// 已经有标题就不动：后来的消息不该把这一行改掉——它是这个会话在列表里的名字，不是
/// 最后一次说话的内容。
pub async fn set_title_if_empty_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    title: &str,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let Some(mut row) = get_in(ex, session).await? else {
        return Err(StoreError::NotFound {
            what: format!("session {session}"),
        });
    };
    if !row.title.trim().is_empty() {
        return Ok(());
    }
    row.update()
        .title(title)
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

/// Session 列表（按 id，也就是按创建时间——UUIDv7）。
///
/// `all = false`（默认）只列 `active` 与 `closing`：逻辑删除过的会话默认不列（§8.10；
/// `closing` 要列出来并标注"正在关闭"，它还在服务）。`all = true` 把 `deleted` 也带上
/// ——它是墓碑但还在列表语义里（可 `show`、可 `purge`）。**`purged` 两种都不列**：那一行
/// 只是"这个会话曾经存在"的记录，只在显式查看单个会话时可见（§8.10 的列表列）。
pub async fn list(db: &Db, all: bool) -> Result<Vec<SessionRecord>, StoreError> {
    db.read(move |ex| {
        Box::pin(async move {
            let rows = SessionRow::all().exec(ex).await.map_err(map_toasty)?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                let record = SessionRecord::try_from_row(row)?;
                let visible = match record.state {
                    SessionState::Active | SessionState::Closing => true,
                    SessionState::Deleted => all,
                    SessionState::Purged => false,
                };
                if visible {
                    out.push(record);
                }
            }
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

        let all = list(&db, false).await.unwrap();
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

    async fn ensure(db: &Db, id: &str) {
        let id = id.to_string();
        db.with_write_retry(move |ex| {
            let id = id.clone();
            Box::pin(async move {
                ensure_in(ex, &SessionId::from_raw(id), "api", "p", NOW)
                    .await
                    .map(|_| ())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();
    }

    async fn set_state(db: &Db, id: &str, from: SessionState, to: SessionState) -> bool {
        let id = id.to_string();
        db.with_write_retry(move |ex| {
            let id = id.clone();
            Box::pin(async move { set_state_in(ex, &SessionId::from_raw(id), from, to, NOW).await })
                as BoxFuture<'_, Result<bool, StoreError>>
        })
        .await
        .unwrap()
    }

    /// 新建的会话是 `active`，而且状态变更时刻就是建行时刻（§8.10 第 1 条）。
    #[tokio::test]
    async fn a_new_session_starts_active() {
        let (db, _dir) = temp().await;
        ensure(&db, "sess-1").await;
        let record = get(&db, &SessionId::from_raw("sess-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.state, SessionState::Active);
        assert!(record.accepts_input());
        assert_eq!(record.state_changed_at, NOW);
    }

    /// `set_state_in` 是 CAS：`from` 对不上就什么都不改。
    ///
    /// 重跑一次逻辑删除（`active → closing`）不能再改一次时间；`purge` 的重跑更不能把
    /// `deleted` 盖回 `closing`——那会让一个已经删掉的会话看起来又在服务（§8.10）。
    #[tokio::test]
    async fn set_state_only_advances_from_the_expected_state() {
        let (db, _dir) = temp().await;
        ensure(&db, "sess-1").await;

        assert!(
            set_state(&db, "sess-1", SessionState::Active, SessionState::Closing).await,
            "第一次受理逻辑删除：推进了"
        );
        assert!(
            !set_state(&db, "sess-1", SessionState::Active, SessionState::Closing).await,
            "重跑：状态已经不是 active，什么都不改"
        );
        assert!(
            set_state(&db, "sess-1", SessionState::Closing, SessionState::Deleted).await,
            "没有未完成的 Run 时由 reconcile 推进到 deleted"
        );
        assert!(
            !set_state(&db, "sess-1", SessionState::Closing, SessionState::Deleted).await,
            "重跑：不重复推进"
        );
        // `komo session delete` 的重跑是 `active → closing`：对已经 `deleted` 的行不生效，
        // 所以它**不会把墓碑盖回 `closing`**（§8.10）。
        assert!(
            !set_state(&db, "sess-1", SessionState::Active, SessionState::Closing).await,
            "**不许把 deleted 盖回 closing**"
        );

        // 墓碑只能往回收方向走，而且重跑幂等。
        assert!(
            set_state(&db, "sess-1", SessionState::Deleted, SessionState::Purged).await,
            "deleted → purged 是最后一步"
        );
        assert!(
            !set_state(&db, "sess-1", SessionState::Deleted, SessionState::Purged).await,
            "回收重跑什么都不改"
        );
        assert!(!set_state(&db, "sess-1", SessionState::Purged, SessionState::Active).await);

        let record = get(&db, &SessionId::from_raw("sess-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.state, SessionState::Purged);
        assert!(!record.accepts_input());
    }

    /// 列里出现认不出的状态值 = 损坏，**不默认成 active**：默认会把墓碑读成活会话。
    #[tokio::test]
    async fn an_unknown_state_value_is_corruption_not_active() {
        let (db, _dir) = temp().await;
        ensure(&db, "sess-1").await;
        let bad = db.clone();
        bad.with_write_retry(|ex| {
            Box::pin(async move {
                toasty::sql::statement("UPDATE sessions SET state = 'zombie' WHERE id = 'sess-1'")
                    .exec(ex)
                    .await
                    .map(|_| ())
                    .map_err(map_toasty)
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();

        let error = get(&db, &SessionId::from_raw("sess-1"))
            .await
            .expect_err("认不出的状态要报损坏");
        assert!(
            matches!(&error, StoreError::Corrupt(why) if why.contains("zombie")),
            "错误要说清是哪个值：{error}"
        );
        assert!(state(&db, &SessionId::from_raw("sess-1")).await.is_err());
    }

    /// 默认只列 `active` / `closing`；`all` 带上 `deleted`；`purged` 两种都不列（§8.10）。
    #[tokio::test]
    async fn deleted_sessions_are_hidden_unless_asked_for() {
        let (db, _dir) = temp().await;
        for id in ["sess-active", "sess-closing", "sess-deleted", "sess-purged"] {
            ensure(&db, id).await;
        }
        assert!(
            set_state(
                &db,
                "sess-closing",
                SessionState::Active,
                SessionState::Closing
            )
            .await
        );
        assert!(
            set_state(
                &db,
                "sess-deleted",
                SessionState::Active,
                SessionState::Closing
            )
            .await
        );
        assert!(
            set_state(
                &db,
                "sess-deleted",
                SessionState::Closing,
                SessionState::Deleted
            )
            .await
        );
        assert!(
            set_state(
                &db,
                "sess-purged",
                SessionState::Active,
                SessionState::Closing
            )
            .await
        );
        assert!(
            set_state(
                &db,
                "sess-purged",
                SessionState::Closing,
                SessionState::Deleted
            )
            .await
        );
        assert!(
            set_state(
                &db,
                "sess-purged",
                SessionState::Deleted,
                SessionState::Purged
            )
            .await
        );

        let visible: Vec<String> = list(&db, false)
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.session.to_string())
            .collect();
        assert_eq!(visible, vec!["sess-active", "sess-closing"]);

        let all: Vec<String> = list(&db, true)
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.session.to_string())
            .collect();
        assert_eq!(
            all,
            vec!["sess-active", "sess-closing", "sess-deleted"],
            "墓碑行只在显式查看单个会话时可见"
        );
    }
}
