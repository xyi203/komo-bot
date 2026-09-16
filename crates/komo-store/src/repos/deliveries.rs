//! `deliveries`：主动投递记录（§11.4）。
//!
//! 不是一个 kernel trait 的实现——`Notifier` 在 gateway，它需要的是"先写行再发送、
//! 重启后补发、按 `DeliveryId` 幂等"这三件事的存储面。
//!
//! `Deferred` **不是错误**：渠道此刻无法推送（微信没有回复令牌），行留在 pending，由
//! 下一条入站消息触发冲刷。所以它和 `Pending` 一样会被 [`TursoDeliveryRepo::pending`]
//! 捞回来。

use komo_kernel::traits::{RepoError, StoreError};
use komo_kernel::types::chat::{
    ChannelPeer, ChannelPlatform, Delivery, DeliveryState, DeliveryTarget, Outbound, PeerId,
};
use komo_kernel::types::ids::{ApprovalId, DeliveryId};
use time::OffsetDateTime;

use crate::db::{BoxFuture, Db, decode, encode, map_toasty, to_ts};
use crate::models::DeliveryRow;

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
        let id = id.clone();
        let target = target.clone();
        let outbound = outbound.clone();
        self.db
            .with_write_retry(move |ex| {
                let (id, target, outbound) = (id.clone(), target.clone(), outbound.clone());
                Box::pin(async move {
                    if let Some(row) = DeliveryRow::filter_by_id(id.as_str())
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    {
                        return record_from_row(&row);
                    }
                    toasty::create!(DeliveryRow {
                        id: id.as_str(),
                        platform: target.peer.platform.as_str(),
                        chat_id: target.peer.chat_id.as_str(),
                        is_home: target.is_home,
                        outbound: encode(&outbound)?,
                        approval_id: approval_of(&outbound).map(|a| a.to_string()),
                        state: enum_str(&DeliveryState::Pending),
                        attempts: 0_i64,
                        last_error: None as Option<String>,
                        created_at: to_ts(now),
                        updated_at: to_ts(now),
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
    pub async fn pending(
        &self,
        peer: Option<&ChannelPeer>,
    ) -> Result<Vec<DeliveryRecord>, RepoError> {
        let peer = peer.cloned();
        self.db
            .read(move |ex| {
                let peer = peer.clone();
                Box::pin(async move {
                    let mut rows = DeliveryRow::all().exec(ex).await.map_err(map_toasty)?;
                    let sent = enum_str(&DeliveryState::Sent);
                    rows.retain(|row| row.state != sent);
                    if let Some(peer) = &peer {
                        rows.retain(|row| {
                            row.platform == peer.platform.as_str()
                                && row.chat_id == peer.chat_id.as_str()
                        });
                    }
                    rows.sort_by(|a, b| a.id.cmp(&b.id));
                    let mut out = Vec::with_capacity(rows.len());
                    for row in &rows {
                        out.push(record_from_row(row)?);
                    }
                    Ok(out)
                }) as BoxFuture<'_, Result<Vec<DeliveryRecord>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    /// 当初把这条审批**投到过哪几个地方**。
    ///
    /// 「审批请求的投递目标：Run 的来源会话，**加上** home chat（若不同）」（§11.4）。
    /// 决定之后要把 `ApprovalSettled` 投回**每一个**，否则另一头那张卡会一直停在"等你
    /// 回答"——而那个人已经答过了。
    ///
    /// 只看 `ApprovalRequest` 那几行：`ApprovalSettled` 自己也带审批 ID，把它算进来会
    /// 让第二次结算投到自己刚投过的地方。**按目标去重**，同一个会话只回一次。
    pub async fn targets_for_approval(
        &self,
        approval: &ApprovalId,
    ) -> Result<Vec<ChannelPeer>, RepoError> {
        let approval = approval.to_string();
        self.db
            .read(move |ex| {
                let approval = approval.clone();
                Box::pin(async move {
                    let mut rows = DeliveryRow::filter(
                        DeliveryRow::fields().approval_id().eq(approval.as_str()),
                    )
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    rows.sort_by(|a, b| a.id.cmp(&b.id));

                    let mut out: Vec<ChannelPeer> = Vec::new();
                    for row in &rows {
                        // 行里的正文是渠道写下的；解不出来就跳过，不让一行坏数据把结算
                        // 投递整个带走。
                        let Ok(outbound) = serde_json::from_str::<Outbound>(&row.outbound) else {
                            continue;
                        };
                        if !matches!(outbound, Outbound::ApprovalRequest(_)) {
                            continue;
                        }
                        let platform: ChannelPlatform =
                            decode(&format!("\"{}\"", row.platform), "deliveries.platform")?;
                        let peer = ChannelPeer {
                            platform,
                            chat_id: PeerId::new(row.chat_id.clone()),
                        };
                        if !out.contains(&peer) {
                            out.push(peer);
                        }
                    }
                    Ok(out)
                }) as BoxFuture<'_, Result<Vec<ChannelPeer>, StoreError>>
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
                    Ok(Some(record_from_row(&row)?))
                }) as BoxFuture<'_, Result<Option<DeliveryRecord>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }
}

/// 这条外发说的是哪条审批。`ApprovalRequest` 与 `ApprovalSettled` 有，其余没有。
fn approval_of(outbound: &Outbound) -> Option<&ApprovalId> {
    match outbound {
        Outbound::ApprovalRequest(presentation) => Some(&presentation.approval),
        Outbound::ApprovalSettled { approval, .. } => Some(approval),
        _ => None,
    }
}

fn record_from_row(row: &DeliveryRow) -> Result<DeliveryRecord, StoreError> {
    let platform: ChannelPlatform =
        decode(&format!("\"{}\"", row.platform), "deliveries.platform")?;
    Ok(DeliveryRecord {
        id: DeliveryId::from_raw(row.id.clone()),
        target: DeliveryTarget {
            peer: ChannelPeer {
                platform,
                chat_id: PeerId::new(row.chat_id.clone()),
            },
            is_home: row.is_home,
        },
        outbound: decode(&row.outbound, "deliveries.outbound")?,
        state: decode(&format!("\"{}\"", row.state), "deliveries.state")?,
        attempts: row.attempts.max(0) as u32,
        last_error: row.last_error.clone(),
        created_at: crate::db::from_ts(row.created_at),
    })
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

    fn approval_request(approval: &ApprovalId, index: u32) -> Outbound {
        use komo_kernel::types::chat::{ApprovalPresentation, ApprovalScope};
        let plan = komo_kernel::test_support::sample_plan(
            "shell",
            &komo_kernel::types::ids::SessionId::from_raw("sess-1"),
        );
        Outbound::ApprovalRequest(Box::new(ApprovalPresentation {
            approval: approval.clone(),
            short_id: komo_kernel::types::ids::ShortId::from_index(index),
            plan_hash: plan.plan_hash(),
            plan,
            reason: "要人看一眼".into(),
            changes: None,
            evidence: None,
            scopes: vec![ApprovalScope::Once],
            valid_until: None,
        }))
    }

    fn text() -> Outbound {
        Outbound::Text {
            text: "跑完了".into(),
        }
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

    /// 验收 BUG(2)：决定之后要把结算投回当初投过的**每一个**目标（§11.4）。
    #[tokio::test]
    async fn an_approvals_targets_are_every_place_the_request_went() {
        let (db, _dir) = temp().await;
        let repo = TursoDeliveryRepo::new(db);
        let approval = ApprovalId::from_raw("ap-1");

        // 「Run 的来源会话，**加上** home chat（若不同）」。
        let source = ChannelPeer::new(ChannelPlatform::Telegram, "42");
        let home = ChannelPeer::new(ChannelPlatform::Feishu, "oc_home");
        repo.record(
            &DeliveryId::from_raw("d-1"),
            &DeliveryTarget::to_peer(source.clone()),
            &approval_request(&approval, 1),
            NOW,
        )
        .await
        .unwrap();
        repo.record(
            &DeliveryId::from_raw("d-2"),
            &DeliveryTarget::home(home.clone()),
            &approval_request(&approval, 1),
            NOW,
        )
        .await
        .unwrap();
        // 别的审批、别的会话——不该混进来。
        repo.record(
            &DeliveryId::from_raw("d-3"),
            &DeliveryTarget::to_peer(ChannelPeer::new(ChannelPlatform::Wechat, "wxid_x")),
            &approval_request(&ApprovalId::from_raw("ap-2"), 2),
            NOW,
        )
        .await
        .unwrap();
        // 一条普通文本，不带审批 ID。
        repo.record(
            &DeliveryId::from_raw("d-4"),
            &DeliveryTarget::to_peer(ChannelPeer::new(ChannelPlatform::Wechat, "wxid_y")),
            &text(),
            NOW,
        )
        .await
        .unwrap();

        let targets = repo.targets_for_approval(&approval).await.unwrap();
        assert_eq!(targets, vec![source.clone(), home.clone()]);

        // 结算投出去之后再问一次：不能把自己刚投过的地方也算进来，否则第二次结算会
        // 再投一轮。
        repo.record(
            &DeliveryId::from_raw("d-5"),
            &DeliveryTarget::to_peer(source.clone()),
            &Outbound::ApprovalSettled {
                approval: approval.clone(),
                short_id: komo_kernel::types::ids::ShortId::from_index(1),
                approved: true,
                by: PeerId::new("operator"),
                at: NOW,
            },
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(
            repo.targets_for_approval(&approval).await.unwrap(),
            vec![source, home]
        );
    }

    /// 同一个会话投过两次也只回一次。
    #[tokio::test]
    async fn a_target_that_was_written_twice_is_only_returned_once() {
        let (db, _dir) = temp().await;
        let repo = TursoDeliveryRepo::new(db);
        let approval = ApprovalId::from_raw("ap-1");
        let peer = ChannelPeer::new(ChannelPlatform::Telegram, "42");
        for id in ["d-1", "d-2"] {
            repo.record(
                &DeliveryId::from_raw(id),
                &DeliveryTarget::to_peer(peer.clone()),
                &approval_request(&approval, 1),
                NOW,
            )
            .await
            .unwrap();
        }
        assert_eq!(
            repo.targets_for_approval(&approval).await.unwrap(),
            vec![peer]
        );
    }

    #[tokio::test]
    async fn an_approval_nobody_delivered_has_no_targets() {
        let (db, _dir) = temp().await;
        let repo = TursoDeliveryRepo::new(db);
        assert!(
            repo.targets_for_approval(&ApprovalId::from_raw("ap-9"))
                .await
                .unwrap()
                .is_empty()
        );
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
}
