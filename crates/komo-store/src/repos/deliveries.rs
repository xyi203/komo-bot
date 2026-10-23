//! `deliveries`：主动投递记录（§11.4）。
//!
//! 不是一个 kernel trait 的实现——`Notifier` 在 gateway，它需要的是"先写行再发送、
//! 重启后补发、按 `DeliveryId` 幂等"这三件事的存储面。
//!
//! `Deferred` **不是错误**：渠道此刻无法推送（微信没有回复令牌），行留在 pending，由
//! 下一条入站消息触发冲刷。所以它和 `Pending` 一样会被 [`TursoDeliveryRepo::pending`]
//! 捞回来。
//!
//! **已作废的旧行**（D3）：删审批之前落盘的 `outbound.kind = approval_request /
//! approval_settled` 不再有意义。读到它们时状态改写成 [`RETIRED_STATE`]（不再投递）并跳过
//! ——不当错误、不重试、不卡补发循环。`approval_id` 列留着、永远写 NULL（§8.2 退役列）。

use komo_kernel::traits::{RepoError, StoreError};
use komo_kernel::types::chat::{
    ChannelPeer, ChannelPlatform, Delivery, DeliveryState, DeliveryTarget, Outbound, PeerId,
};
use komo_kernel::types::ids::DeliveryId;
use time::OffsetDateTime;

use crate::db::{BoxFuture, Db, decode, encode, map_toasty, to_ts};
use crate::models::DeliveryRow;

/// `deliveries.state` 里 store 自己用的第四个值：这一行**不再投递**（D3）。
///
/// 只落在已作废的旧审批行上；不进 [`DeliveryState`]——它不是投递的结局，是"这行已经
/// 没有可投递的正文"。[`TursoDeliveryRepo::pending`] 与 [`TursoDeliveryRepo::get`] 都不再
/// 返回这样的行。
pub const RETIRED_STATE: &str = "retired";

/// 这一行的状态说它**不用再送**了吗：已送到，或已作废。
pub fn is_settled_state(state: &str) -> bool {
    state == RETIRED_STATE || state == enum_str(&DeliveryState::Sent)
}

/// 一条投递记录，读出来的样子。
#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryRecord {
    pub id: DeliveryId,
    pub target: DeliveryTarget,
    pub outbound: Outbound,
    pub state: DeliveryState,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created_at: OffsetDateTime,
}

impl DeliveryRecord {
    pub fn delivery(&self) -> Delivery {
        Delivery {
            id: self.id.clone(),
            state: self.state,
        }
    }
}

/// Turso 上的投递记录。
#[derive(Debug, Clone)]
pub struct TursoDeliveryRepo {
    db: Db,
}

impl TursoDeliveryRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// **先持久化投递记录再发送**（§11.4）。同一个 `id` 重复登记是幂等的。
    pub async fn record(
        &self,
        id: &DeliveryId,
        target: &DeliveryTarget,
        outbound: &Outbound,
        now: OffsetDateTime,
    ) -> Result<DeliveryRecord, RepoError> {
        let key = match outbound {
            Outbound::RunFinished { session, run, .. } => Some(format!("finished:{session}:{run}")),
            Outbound::NeedsAttention { session, run, .. } => {
                Some(format!("attention:{session}:{run}"))
            }
            Outbound::RunAbsorbed { session, run, .. } => Some(format!("absorbed:{session}:{run}")),
            Outbound::Text { .. } => None,
        };
        self.record_once(id, target, outbound, key.as_deref(), now)
            .await
    }

    /// 同一逻辑通知在同一目标只保留一行；pending/deferred 也复用。
    pub async fn record_once(
        &self,
        id: &DeliveryId,
        target: &DeliveryTarget,
        outbound: &Outbound,
        key: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<DeliveryRecord, RepoError> {
        let key = key.map(str::to_owned);
        let id = id.clone();
        let target = target.clone();
        let outbound = outbound.clone();
        self.db
            .with_write_retry(move |ex| {
                let (id, target, outbound) = (id.clone(), target.clone(), outbound.clone());
                let key = key.clone();
                Box::pin(async move {
                    if let Some(row) = DeliveryRow::filter_by_id(id.as_str())
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    {
                        return record_from_row(&row)?.ok_or_else(|| {
                            StoreError::Other(format!("投递 {id} 是一条已作废的旧审批行"))
                        });
                    }
                    if let Some(key) = &key {
                        let rows = DeliveryRow::filter(
                            DeliveryRow::fields()
                                .platform()
                                .eq(target.peer.platform.as_str())
                                .and(
                                    DeliveryRow::fields()
                                        .chat_id()
                                        .eq(target.peer.chat_id.as_str()),
                                ),
                        )
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                        for row in rows {
                            // 同一群里不同话题是不同目标。
                            if row.thread.as_deref()
                                != target.peer.thread.as_ref().map(PeerId::as_str)
                            {
                                continue;
                            }
                            // 旧版记录没有 key；完成 / 介入通知仍按原正文里的身份识别。
                            // 已作废的旧审批行解出来是 `None`，谁也不匹配。
                            let legacy_matches = if row.notification_key.is_none() {
                                match (
                                    Outbound::from_stored(&row.outbound).ok().flatten(),
                                    &outbound,
                                ) {
                                    (
                                        Some(Outbound::RunFinished {
                                            session: a, run: b, ..
                                        }),
                                        Outbound::RunFinished { session, run, .. },
                                    ) => &a == session && &b == run,
                                    (
                                        Some(Outbound::NeedsAttention {
                                            session: a, run: b, ..
                                        }),
                                        Outbound::NeedsAttention { session, run, .. },
                                    ) => &a == session && &b == run,
                                    _ => false,
                                }
                            } else {
                                false
                            };
                            if row.notification_key.as_ref() == Some(key) || legacy_matches {
                                // 带 key 的行不会是旧审批行（它们的 key 是 `approval:` /
                                // `settled:`，新通知生成不出来），解不出就照常往下建新行。
                                if let Some(record) = record_from_row(&row)? {
                                    return Ok(record);
                                }
                            }
                        }
                    }
                    toasty::create!(DeliveryRow {
                        id: id.as_str(),
                        platform: target.peer.platform.as_str(),
                        chat_id: target.peer.chat_id.as_str(),
                        is_home: target.is_home,
                        outbound: encode(&outbound)?,
                        // 退役列（审批已删，D4）：永远 NULL。
                        approval_id: None as Option<String>,
                        notification_key: key,
                        state: enum_str(&DeliveryState::Pending),
                        attempts: 0_i64,
                        last_error: None as Option<String>,
                        created_at: to_ts(now),
                        updated_at: to_ts(now),
                        thread: target.peer.thread.as_ref().map(|t| t.as_str().to_string()),
                    })
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    Ok(DeliveryRecord {
                        id,
                        target,
                        outbound,
                        state: DeliveryState::Pending,
                        attempts: 0,
                        last_error: None,
                        created_at: now,
                    })
                }) as BoxFuture<'_, Result<DeliveryRecord, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    /// 送到了 / 推不出去 / 又失败了一次。
    pub async fn settle(
        &self,
        id: &DeliveryId,
        state: DeliveryState,
        error: Option<String>,
        now: OffsetDateTime,
    ) -> Result<(), RepoError> {
        let id = id.to_string();
        self.db
            .with_write_retry(move |ex| {
                let (id, error) = (id.clone(), error.clone());
                Box::pin(async move {
                    let Some(mut row) = DeliveryRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        return Err(StoreError::NotFound {
                            what: format!("delivery {id}"),
                        });
                    };
                    let attempts = row.attempts + 1;
                    row.update()
                        .state(enum_str(&state))
                        .attempts(attempts)
                        .last_error(error)
                        .updated_at(to_ts(now))
                        .exec(ex)
                        .await
                        .map_err(map_toasty)
                }) as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    /// 还没送到的投递。重启后补发的就是它们，按 [`DeliveryId`] 幂等。
    ///
    /// `peer` 给微信那条路径用：用户下一条消息到达时，Dispatcher 先冲刷**该会话**的
    /// pending 投递，再处理新消息（§11.1 第 4 步）。
    ///
    /// 读到已作废的旧审批行（D3）时**顺手把它们标成 [`RETIRED_STATE`]** 并跳过：返回值里
    /// 永远没有它们，下一次也不会再读到——一条旧行卡不住补发循环。
    pub async fn pending(
        &self,
        peer: Option<&ChannelPeer>,
    ) -> Result<Vec<DeliveryRecord>, RepoError> {
        let peer = peer.cloned();
        let (out, retired) = self
            .db
            .read(move |ex| {
                let peer = peer.clone();
                Box::pin(async move {
                    let mut rows = DeliveryRow::all().exec(ex).await.map_err(map_toasty)?;
                    rows.retain(|row| !is_settled_state(&row.state));
                    if let Some(peer) = &peer {
                        rows.retain(|row| {
                            row.platform == peer.platform.as_str()
                                && row.chat_id == peer.chat_id.as_str()
                                && row.thread.as_deref() == peer.thread.as_ref().map(PeerId::as_str)
                        });
                    }
                    rows.sort_by(|a, b| a.id.cmp(&b.id));
                    let mut out = Vec::with_capacity(rows.len());
                    let mut retired = Vec::new();
                    for row in &rows {
                        match record_from_row(row)? {
                            Some(record) => out.push(record),
                            None => retired.push(row.id.clone()),
                        }
                    }
                    Ok((out, retired))
                })
                    as BoxFuture<'_, Result<(Vec<DeliveryRecord>, Vec<String>), StoreError>>
            })
            .await
            .map_err(RepoError::from)?;
        if !retired.is_empty() {
            self.retire(retired).await?;
        }
        Ok(out)
    }

    /// 把已作废的旧行标成 [`RETIRED_STATE`]。幂等：已经是终态的行不动。
    async fn retire(&self, ids: Vec<String>) -> Result<(), RepoError> {
        let now = to_ts(OffsetDateTime::now_utc());
        self.db
            .with_write_retry(move |ex| {
                let ids = ids.clone();
                Box::pin(async move {
                    for id in &ids {
                        let Some(mut row) = DeliveryRow::filter_by_id(id.as_str())
                            .first()
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?
                        else {
                            continue;
                        };
                        if is_settled_state(&row.state) {
                            continue;
                        }
                        row.update()
                            .state(RETIRED_STATE.to_string())
                            .last_error(Some(
                                "审批已删除（2026-10-11），这条旧审批投递作废，不再投递"
                                    .to_string(),
                            ))
                            .updated_at(now)
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

    pub async fn get(&self, id: &DeliveryId) -> Result<Option<DeliveryRecord>, RepoError> {
        let id = id.to_string();
        self.db
            .read(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    let Some(row) = DeliveryRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        return Ok(None);
                    };
                    // 已作废的旧审批行没有可投递的正文：对调用方来说等于没有这条。
                    record_from_row(&row)
                }) as BoxFuture<'_, Result<Option<DeliveryRecord>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }
}

/// 读一行。`Ok(None)` = 已作废的旧审批行（D3），没有可投递的正文。
fn record_from_row(row: &DeliveryRow) -> Result<Option<DeliveryRecord>, StoreError> {
    if row.state == RETIRED_STATE {
        return Ok(None);
    }
    let Some(outbound) = Outbound::from_stored(&row.outbound).map_err(|e| {
        StoreError::Corrupt(format!("deliveries.outbound 解不开（{}）：{e}", row.id))
    })?
    else {
        return Ok(None);
    };
    let platform: ChannelPlatform =
        decode(&format!("\"{}\"", row.platform), "deliveries.platform")?;
    Ok(Some(DeliveryRecord {
        id: DeliveryId::from_raw(row.id.clone()),
        target: DeliveryTarget {
            peer: ChannelPeer {
                platform,
                chat_id: PeerId::new(row.chat_id.clone()),
                thread: row.thread.clone().map(PeerId::new),
            },
            is_home: row.is_home,
        },
        outbound,
        state: decode(&format!("\"{}\"", row.state), "deliveries.state")?,
        attempts: row.attempts.max(0) as u32,
        last_error: row.last_error.clone(),
        created_at: crate::db::from_ts(row.created_at),
    }))
}

fn enum_str<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .expect("这个枚举序列化成一个字符串")
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-15 08:00:00 UTC);

    async fn temp() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect(dir.path().join("state.db"))
            .await
            .expect("打开库");
        (db, dir)
    }

    fn target(platform: ChannelPlatform, chat: &str, is_home: bool) -> DeliveryTarget {
        DeliveryTarget {
            peer: ChannelPeer::new(platform, chat),
            is_home,
        }
    }

    fn attention(run: &str) -> Outbound {
        Outbound::NeedsAttention {
            session: komo_kernel::types::ids::SessionId::from_raw("sess-1"),
            run: komo_kernel::types::ids::RunId::from_raw(run),
            reason: "结果不明".into(),
        }
    }

    /// 删审批之前落盘的一行：`outbound.kind = approval_request`，状态 pending。
    async fn legacy_approval_row(db: &Db, id: &str, kind: &str) {
        let (id, kind) = (id.to_string(), kind.to_string());
        db.with_write_retry(move |ex| {
            let (id, kind) = (id.clone(), kind.clone());
            Box::pin(async move {
                toasty::create!(DeliveryRow {
                    id: id.as_str(),
                    platform: "telegram",
                    chat_id: "42",
                    is_home: false,
                    outbound: format!(
                        r#"{{"kind":"{kind}","approval":"ap-1","short_id":"K7QF","reason":"x"}}"#
                    ),
                    approval_id: Some("ap-1".to_string()),
                    notification_key: Some("approval:ap-1".to_string()),
                    state: "pending",
                    attempts: 0_i64,
                    last_error: None as Option<String>,
                    created_at: to_ts(NOW),
                    updated_at: to_ts(NOW),
                })
                .exec(ex)
                .await
                .map_err(map_toasty)?;
                Ok(())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();
    }

    fn text() -> Outbound {
        Outbound::Text {
            text: "跑完了".into(),
        }
    }

    #[tokio::test]
    async fn logical_notifications_survive_reopen_and_ignore_home_role_and_new_body() {
        use komo_kernel::types::ids::{RunId, SessionId};
        let (db, dir) = temp().await;
        let repo = TursoDeliveryRepo::new(db);
        let source = target(ChannelPlatform::Telegram, "42", false);
        let messages = [
            Outbound::RunFinished {
                session: SessionId::from_raw("s"),
                run: RunId::from_raw("r"),
                summary: "done".into(),
            },
            attention("r"),
        ];
        // 模拟升级前没有 notification_key 的已送达记录。
        for (index, message) in messages.iter().enumerate() {
            let id = DeliveryId::from_raw(format!("legacy-{index}"));
            repo.record_once(&id, &source, message, None, NOW)
                .await
                .unwrap();
            repo.settle(&id, DeliveryState::Sent, None, NOW)
                .await
                .unwrap();
        }
        drop(repo);
        let repo = TursoDeliveryRepo::new(Db::connect(dir.path().join("state.db")).await.unwrap());
        for (index, message) in messages.iter().enumerate() {
            let record = repo
                .record(
                    &DeliveryId::from_raw(format!("new-{index}")),
                    &target(ChannelPlatform::Telegram, "42", true),
                    message,
                    NOW,
                )
                .await
                .unwrap();
            assert_eq!(record.id, DeliveryId::from_raw(format!("legacy-{index}")));
            assert_eq!(record.state, DeliveryState::Sent);
            // 另一目标仍需投递。
            let other = repo
                .record(
                    &DeliveryId::from_raw(format!("other-{index}")),
                    &target(ChannelPlatform::Telegram, "43", true),
                    message,
                    NOW,
                )
                .await
                .unwrap();
            assert_eq!(other.state, DeliveryState::Pending);
        }
        let changed = Outbound::RunFinished {
            session: SessionId::from_raw("s"),
            run: RunId::from_raw("r"),
            summary: "different wording".into(),
        };
        assert_eq!(
            repo.record(&DeliveryId::from_raw("changed"), &source, &changed, NOW)
                .await
                .unwrap()
                .id,
            DeliveryId::from_raw("legacy-0")
        );
        assert_eq!(repo.pending(None).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_deferred_logical_notification_reuses_its_record() {
        let (db, _dir) = temp().await;
        let repo = TursoDeliveryRepo::new(db);
        let target = target(ChannelPlatform::Wechat, "42", false);
        let message = attention("r");
        let id = DeliveryId::from_raw("original");
        repo.record(&id, &target, &message, NOW).await.unwrap();
        repo.settle(&id, DeliveryState::Deferred, Some("no token".into()), NOW)
            .await
            .unwrap();
        let retry = repo
            .record(&DeliveryId::from_raw("retry"), &target, &message, NOW)
            .await
            .unwrap();
        assert_eq!(retry.id, id);
        assert_eq!(retry.state, DeliveryState::Deferred);
        assert_eq!(repo.pending(None).await.unwrap().len(), 1);
    }

    /// **先持久化投递记录再发送**（§11.4）：行先写，状态 `pending`。
    #[tokio::test]
    async fn a_delivery_is_written_before_it_is_sent() {
        let (db, _dir) = temp().await;
        let repo = TursoDeliveryRepo::new(db);
        let id = DeliveryId::from_raw("d-1");
        let record = repo
            .record(
                &id,
                &target(ChannelPlatform::Telegram, "42", false),
                &text(),
                NOW,
            )
            .await
            .unwrap();
        assert_eq!(record.state, DeliveryState::Pending);
        assert_eq!(record.delivery().state, DeliveryState::Pending);
        assert_eq!(repo.pending(None).await.unwrap().len(), 1);

        repo.settle(&id, DeliveryState::Sent, None, NOW)
            .await
            .unwrap();
        assert!(repo.pending(None).await.unwrap().is_empty());
        assert_eq!(repo.get(&id).await.unwrap().unwrap().attempts, 1);
    }

    /// 按 `DeliveryId` 幂等——补发不会变成两条。
    #[tokio::test]
    async fn recording_the_same_delivery_twice_is_idempotent() {
        let (db, _dir) = temp().await;
        let repo = TursoDeliveryRepo::new(db);
        let id = DeliveryId::from_raw("d-1");
        let target = target(ChannelPlatform::Feishu, "oc_x", true);
        repo.record(&id, &target, &text(), NOW).await.unwrap();
        repo.record(&id, &target, &text(), NOW).await.unwrap();
        assert_eq!(repo.pending(None).await.unwrap().len(), 1);
    }

    /// `Deferred` **不是错误**：行留在 pending，等下一条入站消息冲刷（§11.4）。
    #[tokio::test]
    async fn a_deferred_delivery_stays_in_the_pending_set() {
        let (db, _dir) = temp().await;
        let repo = TursoDeliveryRepo::new(db);
        let id = DeliveryId::from_raw("d-1");
        let peer = ChannelPeer::new(ChannelPlatform::Wechat, "wxid_x");
        repo.record(&id, &DeliveryTarget::to_peer(peer.clone()), &text(), NOW)
            .await
            .unwrap();
        repo.settle(
            &id,
            DeliveryState::Deferred,
            Some("没有回复令牌".into()),
            NOW,
        )
        .await
        .unwrap();

        let pending = repo.pending(None).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].state, DeliveryState::Deferred);

        // 冲刷时按会话取——用户下一条消息到了才轮到这一条。
        assert_eq!(repo.pending(Some(&peer)).await.unwrap().len(), 1);
        assert!(
            repo.pending(Some(&ChannelPeer::new(ChannelPlatform::Wechat, "wxid_y")))
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// D3：删审批之前的 pending 审批行不卡住补发——`pending()` 不返回它、把它标成作废，
    /// 之后 `get` 也读不到；同一目标上别的投递照常补发。
    #[tokio::test]
    async fn a_pending_legacy_approval_delivery_is_retired_instead_of_blocking_resends() {
        let (db, _dir) = temp().await;
        legacy_approval_row(&db, "d-0", "approval_request").await;
        legacy_approval_row(&db, "d-00", "approval_settled").await;
        let repo = TursoDeliveryRepo::new(db.clone());
        let id = DeliveryId::from_raw("d-1");
        repo.record(
            &id,
            &target(ChannelPlatform::Telegram, "42", false),
            &text(),
            NOW,
        )
        .await
        .unwrap();

        let pending = repo.pending(None).await.unwrap();
        assert_eq!(
            pending.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            vec![id.clone()]
        );
        let peer = ChannelPeer::new(ChannelPlatform::Telegram, "42");
        assert_eq!(repo.pending(Some(&peer)).await.unwrap().len(), 1);

        // 作废落进了库：状态不再是 pending，再读也不会出现。
        let row = db
            .read(|ex| {
                Box::pin(async move {
                    DeliveryRow::filter_by_id("d-0")
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)
                }) as BoxFuture<'_, Result<Option<DeliveryRow>, StoreError>>
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, RETIRED_STATE);
        assert!(row.last_error.is_some());
        assert!(
            repo.get(&DeliveryId::from_raw("d-0"))
                .await
                .unwrap()
                .is_none()
        );

        // 新的介入通知不会被旧行的 key 吞掉。
        let fresh = repo
            .record(
                &DeliveryId::from_raw("d-2"),
                &target(ChannelPlatform::Telegram, "42", false),
                &attention("r-1"),
                NOW,
            )
            .await
            .unwrap();
        assert_eq!(fresh.id, DeliveryId::from_raw("d-2"));
        assert_eq!(repo.pending(None).await.unwrap().len(), 2);
    }

    /// 内容原样读得回来——补发一条结果消息不需要重跑任务（§8.8）。
    #[tokio::test]
    async fn the_outbound_body_survives_a_restart() {
        let (db, _dir) = temp().await;
        let repo = TursoDeliveryRepo::new(db);
        let id = DeliveryId::from_raw("d-1");
        let body = Outbound::NeedsAttention {
            session: komo_kernel::types::ids::SessionId::from_raw("sess-1"),
            run: komo_kernel::types::ids::RunId::from_raw("run-1"),
            reason: "结果不明".into(),
        };
        repo.record(
            &id,
            &target(ChannelPlatform::Feishu, "oc_x", true),
            &body,
            NOW,
        )
        .await
        .unwrap();
        let read = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(read.outbound, body);
        assert!(read.target.is_home);
    }

    fn thread_target(thread: Option<&str>) -> DeliveryTarget {
        let peer = ChannelPeer::new(ChannelPlatform::Feishu, "oc_g");
        DeliveryTarget {
            peer: match thread {
                Some(t) => peer.in_thread(t),
                None => peer,
            },
            is_home: false,
        }
    }

    #[tokio::test]
    async fn a_thread_peer_keeps_its_thread_through_the_store() {
        let (db, _dir) = temp().await;
        let repo = TursoDeliveryRepo::new(db);
        let id = DeliveryId::from_raw("d-1");
        repo.record(&id, &thread_target(Some("om_a")), &text(), NOW)
            .await
            .unwrap();
        let read = repo.get(&id).await.unwrap().unwrap();
        assert_eq!(read.target.peer.thread, Some(PeerId::new("om_a")));
    }

    #[tokio::test]
    async fn pending_for_a_peer_only_returns_that_threads_rows() {
        let (db, _dir) = temp().await;
        let repo = TursoDeliveryRepo::new(db);
        for (id, thread) in [("d-a", Some("om_a")), ("d-b", Some("om_b")), ("d-n", None)] {
            repo.record(
                &DeliveryId::from_raw(id),
                &thread_target(thread),
                &text(),
                NOW,
            )
            .await
            .unwrap();
        }
        let ids = |records: Vec<DeliveryRecord>| -> Vec<String> {
            records.iter().map(|r| r.id.to_string()).collect()
        };
        let a = thread_target(Some("om_a")).peer;
        assert_eq!(ids(repo.pending(Some(&a)).await.unwrap()), ["d-a"]);
        let none = thread_target(None).peer;
        assert_eq!(ids(repo.pending(Some(&none)).await.unwrap()), ["d-n"]);
    }

    #[tokio::test]
    async fn the_same_notification_key_in_two_threads_makes_two_rows() {
        let (db, _dir) = temp().await;
        let repo = TursoDeliveryRepo::new(db);
        let body = attention("r-1");
        let a = repo
            .record(
                &DeliveryId::from_raw("d-a"),
                &thread_target(Some("om_a")),
                &body,
                NOW,
            )
            .await
            .unwrap();
        let b = repo
            .record(
                &DeliveryId::from_raw("d-b"),
                &thread_target(Some("om_b")),
                &body,
                NOW,
            )
            .await
            .unwrap();
        let again = repo
            .record(
                &DeliveryId::from_raw("d-c"),
                &thread_target(Some("om_a")),
                &body,
                NOW,
            )
            .await
            .unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(again.id, a.id);
    }
}
