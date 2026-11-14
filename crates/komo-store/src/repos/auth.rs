//! 登录状态仓储（`docs/redesign.md` §2.9）：网页会话 cookie 与设备令牌，落在 `aux.db` 的
//! `auth_sessions`。
//!
//! 只认**令牌的哈希**：Gateway 签发时算一次 SHA-256 交进来，之后每次请求拿 cookie / Bearer 重算
//! 再查。表里没有明文，库文件被读走也换不出登录状态。列表与吊销用 `id`（UUIDv7，与令牌无关）。
//!
//! 过期是读时判的：[`TursoAuthRepo::find`] 对过期的行答 `None`，不顺手删（读不抢写锁）；清理走
//! [`TursoAuthRepo::purge_expired`]。

use komo_kernel::traits::StoreError;
use komo_kernel::types::ids::uuid_v7_at;
use time::OffsetDateTime;

use crate::db::{BoxFuture, Db, from_ts, from_ts_opt, map_toasty, to_ts, to_ts_opt};
use crate::models::AuthSessionRow;

/// 登录状态的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// 浏览器的会话 cookie。
    Web,
    /// 桌面端 / CLI 的长期设备令牌（Bearer）。
    Device,
}

impl AuthKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthKind::Web => "web",
            AuthKind::Device => "device",
        }
    }

    fn parse(raw: &str) -> Option<AuthKind> {
        match raw {
            "web" => Some(AuthKind::Web),
            "device" => Some(AuthKind::Device),
            _ => None,
        }
    }
}

/// 一条登录状态（不含令牌，也不含哈希）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSessionRecord {
    pub id: String,
    pub kind: AuthKind,
    pub subject: String,
    pub email: Option<String>,
    pub label: String,
    pub created_at: OffsetDateTime,
    pub last_used_at: OffsetDateTime,
    /// `None` = 不过期。
    pub expires_at: Option<OffsetDateTime>,
}

/// 新建一条要的东西。
#[derive(Debug, Clone)]
pub struct NewAuthSession {
    pub kind: AuthKind,
    /// 令牌的 SHA-256 十六进制。
    pub token_hash: String,
    pub subject: String,
    pub email: Option<String>,
    pub label: String,
    pub now: OffsetDateTime,
    pub expires_at: Option<OffsetDateTime>,
}

/// Turso 上的登录状态仓储。
#[derive(Debug, Clone)]
pub struct TursoAuthRepo {
    db: Db,
}

impl TursoAuthRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub async fn create(&self, new: NewAuthSession) -> Result<AuthSessionRecord, StoreError> {
        let id = uuid_v7_at(new.now).to_string();
        self.db
            .with_write_retry(move |ex| {
                let new = new.clone();
                let id = id.clone();
                Box::pin(async move {
                    toasty::create!(AuthSessionRow {
                        id: id.clone(),
                        kind: new.kind.as_str(),
                        token_hash: new.token_hash.clone(),
                        subject: new.subject.clone(),
                        email: new.email.clone(),
                        label: new.label.clone(),
                        created_at: to_ts(new.now),
                        last_used_at: to_ts(new.now),
                        expires_at: to_ts_opt(new.expires_at),
                    })
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    Ok(AuthSessionRecord {
                        id,
                        kind: new.kind,
                        subject: new.subject,
                        email: new.email,
                        label: new.label,
                        created_at: new.now,
                        last_used_at: new.now,
                        expires_at: new.expires_at,
                    })
                }) as BoxFuture<'_, Result<AuthSessionRecord, StoreError>>
            })
            .await
    }

    /// 按令牌哈希找一条**没过期、种类对得上**的。
    pub async fn find(
        &self,
        kind: AuthKind,
        token_hash: &str,
        now: OffsetDateTime,
    ) -> Result<Option<AuthSessionRecord>, StoreError> {
        let token_hash = token_hash.to_string();
        self.db
            .read(move |ex| {
                let token_hash = token_hash.clone();
                Box::pin(async move {
                    let rows = AuthSessionRow::filter(
                        AuthSessionRow::fields()
                            .token_hash()
                            .eq(token_hash.as_str()),
                    )
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    Ok(rows
                        .iter()
                        .filter_map(record_of)
                        .find(|record| record.kind == kind && !expired(record, now)))
                }) as BoxFuture<'_, Result<Option<AuthSessionRecord>, StoreError>>
            })
            .await
    }

    /// 记一次使用，顺带（给了的话）把过期时间往后推。已经被吊销的 id 什么都不做。
    pub async fn touch(
        &self,
        id: &str,
        now: OffsetDateTime,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<(), StoreError> {
        let id = id.to_string();
        self.db
            .with_write_retry(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    let Some(mut row) = AuthSessionRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        return Ok(());
                    };
                    let expires = expires_at.map(to_ts).unwrap_or(row.expires_at);
                    row.update()
                        .last_used_at(to_ts(now))
                        .expires_at(expires)
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                    Ok(())
                }) as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
    }

    /// 没过期的全部，最近用过的在前。
    pub async fn list(&self, now: OffsetDateTime) -> Result<Vec<AuthSessionRecord>, StoreError> {
        self.db
            .read(move |ex| {
                Box::pin(async move {
                    let rows = AuthSessionRow::all().exec(ex).await.map_err(map_toasty)?;
                    let mut out: Vec<AuthSessionRecord> = rows
                        .iter()
                        .filter_map(record_of)
                        .filter(|record| !expired(record, now))
                        .collect();
                    out.sort_by(|a, b| {
                        b.last_used_at
                            .cmp(&a.last_used_at)
                            .then_with(|| b.id.cmp(&a.id))
                    });
                    Ok(out)
                }) as BoxFuture<'_, Result<Vec<AuthSessionRecord>, StoreError>>
            })
            .await
    }

    /// 吊销一条；有这条才答 `true`。
    pub async fn revoke(&self, id: &str) -> Result<bool, StoreError> {
        let id = id.to_string();
        self.db
            .with_write_retry(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    let Some(row) = AuthSessionRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        return Ok(false);
                    };
                    row.delete().exec(ex).await.map_err(map_toasty)?;
                    Ok(true)
                }) as BoxFuture<'_, Result<bool, StoreError>>
            })
            .await
    }

    /// 吊销令牌哈希对应的那条（退出登录用）。
    pub async fn revoke_by_hash(
        &self,
        kind: AuthKind,
        token_hash: &str,
    ) -> Result<bool, StoreError> {
        let token_hash = token_hash.to_string();
        self.db
            .with_write_retry(move |ex| {
                let token_hash = token_hash.clone();
                Box::pin(async move {
                    let rows = AuthSessionRow::filter(
                        AuthSessionRow::fields()
                            .token_hash()
                            .eq(token_hash.as_str()),
                    )
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    let mut any = false;
                    for row in rows {
                        if AuthKind::parse(&row.kind) == Some(kind) {
                            row.delete().exec(ex).await.map_err(map_toasty)?;
                            any = true;
                        }
                    }
                    Ok(any)
                }) as BoxFuture<'_, Result<bool, StoreError>>
            })
            .await
    }

    /// 删掉已过期的，返回删了几条。
    pub async fn purge_expired(&self, now: OffsetDateTime) -> Result<usize, StoreError> {
        self.db
            .with_write_retry(move |ex| {
                Box::pin(async move {
                    let rows = AuthSessionRow::all().exec(ex).await.map_err(map_toasty)?;
                    let mut removed = 0usize;
                    for row in rows {
                        let stale = from_ts_opt(row.expires_at).is_some_and(|at| at <= now);
                        if stale {
                            row.delete().exec(ex).await.map_err(map_toasty)?;
                            removed += 1;
                        }
                    }
                    Ok(removed)
                }) as BoxFuture<'_, Result<usize, StoreError>>
            })
            .await
    }
}

fn expired(record: &AuthSessionRecord, now: OffsetDateTime) -> bool {
    record.expires_at.is_some_and(|at| at <= now)
}

/// 读不出种类的行（别的版本写的）跳过，不让它把整张清单带走。
fn record_of(row: &AuthSessionRow) -> Option<AuthSessionRecord> {
    Some(AuthSessionRecord {
        id: row.id.clone(),
        kind: AuthKind::parse(&row.kind)?,
        subject: row.subject.clone(),
        email: row.email.clone(),
        label: row.label.clone(),
        created_at: from_ts(row.created_at),
        last_used_at: from_ts(row.last_used_at),
        expires_at: from_ts_opt(row.expires_at),
    })
}
