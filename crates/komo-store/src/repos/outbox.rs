//! `control_outbox`：控制事务产生、尚待补写到 JSONL 的审计事件（§8.5 的反向顺序）。
//!
//! ```text
//! state.db 事务提交审批决定与 control_outbox（固定 event_id）
//!   → Session 写入器将审计事件追加到 JSONL 并同步
//!   → state.db 标记 outbox 已交付并更新日志索引
//! ```
//!
//! **审批生效不依赖审计补写成功**：Turso 的提交就是 fsync（§8.2 实测），客户端可以在
//! 数据库提交后立刻得到确认，outbox 只承担审计补写与顺序，不承担耐久性。

use komo_kernel::events::EventPayload;
use komo_kernel::traits::StoreError;
use komo_kernel::types::ids::{EventId, Seq, SessionId};
use time::OffsetDateTime;
use toasty::Executor;

use crate::db::{BoxFuture, Db, map_toasty, to_ts};
use crate::models::ControlOutboxRow;

/// 一条待补写的审计事件。
#[derive(Debug, Clone, PartialEq)]
pub struct PendingAudit {
    pub event_id: EventId,
    pub session: SessionId,
    pub payload: EventPayload,
    pub occurred_at: OffsetDateTime,
}

/// 在控制事务里排一条审计事件。**event_id 由调用方固定**，补写按它幂等。
///
/// 没有 `now` 参数：入队时间由这里读钟。`occurred_at` 才是要保留的那个——补写保留**原始
/// 发生时间**，不凭日志行相邻推断审批关系（§8.3），而"什么时候排进队的"只是运维信息。
pub async fn enqueue_in(
    ex: &mut dyn Executor,
    session: &SessionId,
    event_id: &EventId,
    payload: EventPayload,
    occurred_at: OffsetDateTime,
) -> Result<(), StoreError> {
    if ControlOutboxRow::filter_by_id(event_id.as_str())
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?
        .is_some()
    {
        return Ok(());
    }
    let (payload_type, data) = split_payload(&payload)?;
    let now = OffsetDateTime::now_utc();
    toasty::create!(ControlOutboxRow {
        id: event_id.as_str(),
        session_id: session.as_str(),
        payload_type,
        payload: data,
        occurred_at: to_ts(occurred_at),
        delivered: false,
        seq: 0_i64,
        created_at: to_ts(now),
    })
    .exec(ex)
    .await
    .map_err(map_toasty)?;
    Ok(())
}

/// 尚未补写的审计事件，按写入顺序（id 是 UUIDv7）。
pub async fn pending(db: &Db, limit: usize) -> Result<Vec<PendingAudit>, StoreError> {
    db.read(move |ex| {
        Box::pin(async move {
            let mut rows =
                ControlOutboxRow::filter(ControlOutboxRow::fields().delivered().eq(false))
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
            rows.sort_by(|a, b| a.id.cmp(&b.id));
            rows.truncate(limit);

            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                out.push(PendingAudit {
                    event_id: EventId::from_raw(row.id),
                    session: SessionId::from_raw(row.session_id),
                    payload: join_payload(&row.payload_type, &row.payload)?,
                    occurred_at: crate::db::from_ts(row.occurred_at),
                });
            }
            Ok(out)
        }) as BoxFuture<'_, Result<Vec<PendingAudit>, StoreError>>
    })
    .await
}

/// 标记已交付。
///
/// 落在 JSONL 的哪个 seq **从索引读回来**，不要调用方再报一遍：`session_log_index` 那一
/// 行是同一个 `event_id` 在同一个事务里写下的，让它当唯一来源，两处就不会各说各的。
pub async fn mark_delivered_in(
    ex: &mut dyn Executor,
    event_id: &EventId,
) -> Result<(), StoreError> {
    let Some(mut row) = ControlOutboxRow::filter_by_id(event_id.as_str())
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?
    else {
        return Ok(());
    };
    let seq = crate::models::SessionLogIndexRow::filter_by_id(event_id.as_str())
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?
        .map(|indexed| indexed.seq)
        .unwrap_or(0);
    row.update()
        .delivered(true)
        .seq(seq)
        .exec(ex)
        .await
        .map_err(map_toasty)
}

/// `EventPayload` 拆成 `type` 与 `data`——列里存的就是这两样，和 JSONL 的信封同形。
fn split_payload(payload: &EventPayload) -> Result<(String, String), StoreError> {
    let event = komo_kernel::events::Event {
        v: komo_kernel::events::EVENT_FORMAT_VERSION,
        seq: Seq::ZERO,
        event_id: EventId::from_raw("outbox"),
        session: SessionId::from_raw("outbox"),
        run: None,
        ts: OffsetDateTime::UNIX_EPOCH,
        payload: payload.clone(),
    };
    let line = event
        .to_line()
        .map_err(|e| StoreError::Other(format!("审计事件序列化失败：{e}")))?;
    let value: serde_json::Value = serde_json::from_str(&line)
        .map_err(|e| StoreError::Other(format!("审计事件序列化失败：{e}")))?;
    let data = value
        .get("data")
        .cloned()
        .unwrap_or(serde_json::Value::Null)
        .to_string();
    Ok((payload.type_name().to_string(), data))
}

fn join_payload(payload_type: &str, data: &str) -> Result<EventPayload, StoreError> {
    let line = serde_json::json!({
        "v": komo_kernel::events::EVENT_FORMAT_VERSION,
        "seq": 0,
        "event_id": "outbox",
        "session_id": "outbox",
        "at": "1970-01-01T00:00:00Z",
        "type": payload_type,
        "data": serde_json::from_str::<serde_json::Value>(data)
            .map_err(|e| StoreError::Corrupt(format!("control_outbox.payload 不是 JSON：{e}")))?,
    })
    .to_string();
    komo_kernel::events::Event::from_line(&line)
        .map(|event| event.payload)
        .map_err(|e| StoreError::Corrupt(format!("control_outbox 的事件解析失败：{e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::events::ApprovalDecided;
    use komo_kernel::types::chat::{ApprovalScope, PeerId};
    use komo_kernel::types::ids::ApprovalId;
    use time::macros::datetime;

    const DECIDED_AT: OffsetDateTime = datetime!(2026-09-14 20:30:00 UTC);

    async fn temp() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect(dir.path().join("state.db"))
            .await
            .expect("打开库");
        (db, dir)
    }

    fn payload() -> EventPayload {
        EventPayload::ApprovalDecided(ApprovalDecided {
            approval: ApprovalId::from_raw("ap-1"),
            approved: true,
            scope: ApprovalScope::Once,
            by: Some(PeerId::new("operator")),
            decided_at: DECIDED_AT,
            grant: None,
        })
    }

    async fn enqueue(db: &Db, id: &str) {
        let id = id.to_string();
        db.with_write_retry(move |ex| {
            let id = id.clone();
            Box::pin(async move {
                enqueue_in(
                    ex,
                    &SessionId::from_raw("sess-1"),
                    &EventId::from_raw(id),
                    payload(),
                    DECIDED_AT,
                )
                .await
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();
    }

    /// 排队、取出、标记已交付——§8.5 反向顺序的三步。
    #[tokio::test]
    async fn an_audit_event_round_trips_with_its_original_time() {
        let (db, _dir) = temp().await;
        enqueue(&db, "evt-1").await;

        let waiting = pending(&db, 10).await.unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].event_id.as_str(), "evt-1");
        assert_eq!(
            waiting[0].occurred_at, DECIDED_AT,
            "保留原始发生时间，不凭日志行相邻推断审批关系"
        );
        assert_eq!(waiting[0].payload, payload());

        db.with_write_retry(|ex| {
            Box::pin(async move { mark_delivered_in(ex, &EventId::from_raw("evt-1")).await })
                as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();
        assert!(pending(&db, 10).await.unwrap().is_empty());
    }

    /// **event_id 由调用方固定**，排队是幂等的。
    #[tokio::test]
    async fn enqueueing_the_same_event_twice_keeps_one_row() {
        let (db, _dir) = temp().await;
        enqueue(&db, "evt-1").await;
        enqueue(&db, "evt-1").await;
        assert_eq!(pending(&db, 10).await.unwrap().len(), 1);
    }

    /// 词汇里没有的 `type` 也存得下、读得回——降级运行的进程写下的行不能让补写卡住。
    #[tokio::test]
    async fn an_unknown_payload_type_survives_the_round_trip() {
        let (db, _dir) = temp().await;
        let unknown = EventPayload::Unknown {
            event_type: "approval.escalated".into(),
            raw: serde_json::json!({"note": "未来的机制"}),
        };
        let cloned = unknown.clone();
        db.with_write_retry(move |ex| {
            let unknown = cloned.clone();
            Box::pin(async move {
                enqueue_in(
                    ex,
                    &SessionId::from_raw("sess-1"),
                    &EventId::from_raw("evt-x"),
                    unknown,
                    DECIDED_AT,
                )
                .await
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();

        let waiting = pending(&db, 10).await.unwrap();
        assert_eq!(waiting[0].payload, unknown);
    }
}
