//! 浏览器推送订阅仓储（`docs/redesign.md` §1.8）：`aux.db` 的 `push_subscriptions`。

use komo_kernel::traits::StoreError;
use komo_kernel::types::ids::uuid_v7_at;
use time::OffsetDateTime;

use crate::db::{BoxFuture, Db, from_ts, map_toasty, to_ts};
use crate::models::PushSubscriptionRow;

/// 一条订阅。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushSubscription {
    pub id: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub user_agent: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// 订阅时要存的东西。
#[derive(Debug, Clone)]
pub struct NewPushSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub user_agent: Option<String>,
    pub now: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub struct TursoPushRepo {
    db: Db,
}

impl TursoPushRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// 同一个 `endpoint` 已经有就更新密钥与 UA（`id` 与 `created_at` 不变），没有就新建。返回那一行的 `id`。
    pub async fn upsert(&self, new: NewPushSubscription) -> Result<String, StoreError> {
        let fresh_id = uuid_v7_at(new.now).to_string();
        self.db
            .with_write_retry(move |ex| {
                let new = new.clone();
                let fresh_id = fresh_id.clone();
                Box::pin(async move {
                    let existing = PushSubscriptionRow::filter(
                        PushSubscriptionRow::fields()
                            .endpoint()
                            .eq(new.endpoint.as_str()),
                    )
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    if let Some(mut row) = existing.into_iter().next() {
                        let id = row.id.clone();
                        row.update()
                            .p256dh(new.p256dh.clone())
                            .auth(new.auth.clone())
                            .user_agent(new.user_agent.clone())
                            .updated_at(to_ts(new.now))
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?;
                        return Ok(id);
                    }
                    toasty::create!(PushSubscriptionRow {
                        id: fresh_id.clone(),
                        endpoint: new.endpoint.clone(),
                        p256dh: new.p256dh.clone(),
                        auth: new.auth.clone(),
                        user_agent: new.user_agent.clone(),
                        created_at: to_ts(new.now),
                        updated_at: to_ts(new.now),
                    })
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    Ok(fresh_id)
                }) as BoxFuture<'_, Result<String, StoreError>>
            })
            .await
    }

    /// 全部订阅，最早订阅的在前。
    pub async fn list(&self) -> Result<Vec<PushSubscription>, StoreError> {
        self.db
            .read(move |ex| {
                Box::pin(async move {
                    let rows = PushSubscriptionRow::all()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                    let mut out: Vec<PushSubscription> = rows
                        .iter()
                        .map(|row| PushSubscription {
                            id: row.id.clone(),
                            endpoint: row.endpoint.clone(),
                            p256dh: row.p256dh.clone(),
                            auth: row.auth.clone(),
                            user_agent: row.user_agent.clone(),
                            created_at: from_ts(row.created_at),
                            updated_at: from_ts(row.updated_at),
                        })
                        .collect();
                    out.sort_by(|a, b| {
                        a.created_at
                            .cmp(&b.created_at)
                            .then_with(|| a.id.cmp(&b.id))
                    });
                    Ok(out)
                }) as BoxFuture<'_, Result<Vec<PushSubscription>, StoreError>>
            })
            .await
    }

    /// 按 endpoint 删；有这条才答 `true`。
    pub async fn remove(&self, endpoint: &str) -> Result<bool, StoreError> {
        let endpoint = endpoint.to_string();
        self.db
            .with_write_retry(move |ex| {
                let endpoint = endpoint.clone();
                Box::pin(async move {
                    let rows = PushSubscriptionRow::filter(
                        PushSubscriptionRow::fields()
                            .endpoint()
                            .eq(endpoint.as_str()),
                    )
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    let mut any = false;
                    for row in rows {
                        row.delete().exec(ex).await.map_err(map_toasty)?;
                        any = true;
                    }
                    Ok(any)
                }) as BoxFuture<'_, Result<bool, StoreError>>
            })
            .await
    }
}
