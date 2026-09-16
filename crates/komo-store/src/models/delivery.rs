//! `deliveries`：主动投递记录（§11.4）。耐久表。

use super::{ColumnSpec, TableSpec};

/// 一条主动投递。
///
/// **先写行再发送**：`pending` 的行重启后补发，按主键幂等；`deferred` 是"渠道此刻推
/// 不出去"（微信没有回复令牌），不是错误（§11.4）。
#[derive(Debug, toasty::Model)]
#[table = "deliveries"]
pub struct DeliveryRow {
    #[key]
    pub id: String,
    pub platform: String,
    pub chat_id: String,
    /// 这条是不是补送到 home chat 的那一份。
    pub is_home: bool,
    /// `Outbound` 的 JSON。
    pub outbound: String,
    /// 这条投递说的是哪条审批（只有 `ApprovalRequest` / `ApprovalSettled` 有）。
    ///
    /// 它是从 `outbound` 里提出来的**同一个值**，另开一列只为了能在 SQL 里筛：决定之后
    /// 要把 `ApprovalSettled` 投到当初 `ApprovalRequest` 投过的**每一个**目标，那是一次
    /// 按审批 ID 的查询（§11.4）。
    pub approval_id: Option<String>,
    /// `DeliveryState`：pending / sent / deferred。
    pub state: String,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub const SPEC: TableSpec = TableSpec {
    name: "deliveries",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "deliveries" ("id" TEXT NOT NULL, "platform" TEXT NOT NULL, "chat_id" TEXT NOT NULL, "is_home" BOOLEAN NOT NULL, "outbound" TEXT NOT NULL, "approval_id" TEXT, "state" TEXT NOT NULL, "attempts" BIGINT NOT NULL, "last_error" TEXT, "created_at" BIGINT NOT NULL, "updated_at" BIGINT NOT NULL, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("platform", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("chat_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("is_home", "BOOLEAN NOT NULL DEFAULT 0"),
    ColumnSpec::new("outbound", "TEXT NOT NULL DEFAULT '{}'"),
    ColumnSpec::new("approval_id", "TEXT"),
    ColumnSpec::new("state", "TEXT NOT NULL DEFAULT 'pending'"),
    ColumnSpec::new("attempts", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("last_error", "TEXT"),
    ColumnSpec::new("created_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("updated_at", "BIGINT NOT NULL DEFAULT 0"),
];
