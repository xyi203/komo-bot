//! `auth_sessions`：登录状态（`docs/redesign.md` §2.9）。耐久表。
//!
//! 网页的会话 cookie 与桌面端 / CLI 的设备令牌各占一行，`kind` 区分。**只存令牌的 SHA-256**
//! （`token_hash`），明文只在签发那一刻出现在响应里；列表与吊销用不含令牌信息的 `id`。
//! 注意与 `durable.db` 的 `sessions`（对话会话）不是一回事。

use super::{ColumnSpec, TableSpec};

#[derive(Debug, toasty::Model)]
#[table = "auth_sessions"]
pub struct AuthSessionRow {
    #[key]
    pub id: String,
    /// `web` / `device`。
    pub kind: String,
    /// 令牌的 SHA-256 十六进制；唯一索引 `auth_sessions_token`。
    pub token_hash: String,
    /// OIDC 的 `sub`。
    pub subject: String,
    pub email: Option<String>,
    /// 给人看的名字：网页是 User-Agent 摘要，设备是客户端报的名字（`komo-cli` / `Komo Desktop` …）。
    pub label: String,
    pub created_at: i64,
    pub last_used_at: i64,
    /// `0` = 不过期（设备令牌）；网页会话有滑动的过期时间。
    pub expires_at: i64,
}

pub const SPEC: TableSpec = TableSpec {
    name: "auth_sessions",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "auth_sessions" ("id" TEXT NOT NULL, "kind" TEXT NOT NULL, "token_hash" TEXT NOT NULL, "subject" TEXT NOT NULL, "email" TEXT, "label" TEXT NOT NULL, "created_at" BIGINT NOT NULL, "last_used_at" BIGINT NOT NULL, "expires_at" BIGINT NOT NULL, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("kind", "TEXT NOT NULL DEFAULT 'web'"),
    ColumnSpec::new("token_hash", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("subject", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("email", "TEXT"),
    ColumnSpec::new("label", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("created_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("last_used_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("expires_at", "BIGINT NOT NULL DEFAULT 0"),
];
