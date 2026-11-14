//! `push_subscriptions`：浏览器推送的订阅（`docs/redesign.md` §1.8）。耐久表。
//!
//! 一台设备（一个浏览器 profile）一行，`endpoint` 是推送服务给的地址（唯一索引
//! `push_subscriptions_endpoint`），同一个 endpoint 再订阅是更新。`p256dh` / `auth` 是浏览器的公钥与
//! 认证密钥（base64url，原样存），发送时加密负载要用；它们只能加密发给这台设备的消息，不是登录凭据。

use super::{ColumnSpec, TableSpec};

#[derive(Debug, toasty::Model)]
#[table = "push_subscriptions"]
pub struct PushSubscriptionRow {
    #[key]
    pub id: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub user_agent: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub const SPEC: TableSpec = TableSpec {
    name: "push_subscriptions",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "push_subscriptions" ("id" TEXT NOT NULL, "endpoint" TEXT NOT NULL, "p256dh" TEXT NOT NULL, "auth" TEXT NOT NULL, "user_agent" TEXT, "created_at" BIGINT NOT NULL, "updated_at" BIGINT NOT NULL, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("endpoint", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("p256dh", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("auth", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("user_agent", "TEXT"),
    ColumnSpec::new("created_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("updated_at", "BIGINT NOT NULL DEFAULT 0"),
];
