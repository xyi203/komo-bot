//! `approval_requests`：审批的**权威**记录（§7.4、§8.2）。耐久表，只允许加法变更。

use super::{ColumnSpec, TableSpec};

/// 一条审批请求。
///
/// `decided` 是一个独立的布尔列而不是"`decision` 是否为 NULL"：短 ID 只在**待处理
/// 集合内**唯一（§11.3），所以按短 ID 查找必须是一个能走索引的谓词。
#[derive(Debug, toasty::Model)]
#[table = "approval_requests"]
pub struct ApprovalRequestRow {
    #[key]
    pub id: String,
    /// 待处理集合内唯一的 4 位 base32 短 ID。
    pub short_id: String,
    pub session_id: String,
    pub run_id: Option<String>,
    pub call_id: Option<String>,
    pub plan_hash: String,
    /// `ExecutionPlan` 的 JSON。§7.2 要求界面显示它。
    pub plan: String,
    pub reason: String,
    pub changes: Option<String>,
    pub evidence: Option<String>,
    /// `Vec<ApprovalScope>` 的 JSON。
    pub scopes: String,
    pub requested_at: i64,
    /// `0` = 不过期。
    pub valid_until: i64,
    pub decided: bool,
    /// `ApprovalDecisionRecord` 的 JSON。
    pub decision: Option<String>,
}

pub const SPEC: TableSpec = TableSpec {
    name: "approval_requests",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "approval_requests" ("id" TEXT NOT NULL, "short_id" TEXT NOT NULL, "session_id" TEXT NOT NULL, "run_id" TEXT, "call_id" TEXT, "plan_hash" TEXT NOT NULL, "plan" TEXT NOT NULL, "reason" TEXT NOT NULL, "changes" TEXT, "evidence" TEXT, "scopes" TEXT NOT NULL, "requested_at" BIGINT NOT NULL, "valid_until" BIGINT NOT NULL, "decided" BOOLEAN NOT NULL, "decision" TEXT, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("short_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("session_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("run_id", "TEXT"),
    ColumnSpec::new("call_id", "TEXT"),
    ColumnSpec::new("plan_hash", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("plan", "TEXT NOT NULL DEFAULT '{}'"),
    ColumnSpec::new("reason", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("changes", "TEXT"),
    ColumnSpec::new("evidence", "TEXT"),
    ColumnSpec::new("scopes", "TEXT NOT NULL DEFAULT '[]'"),
    ColumnSpec::new("requested_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("valid_until", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("decided", "BOOLEAN NOT NULL DEFAULT 0"),
    ColumnSpec::new("decision", "TEXT"),
];
