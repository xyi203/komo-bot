//! `control_outbox`：数据库控制事务产生、尚待补写到 JSONL 的审计事件（§8.2、§8.5）。

use super::{ColumnSpec, TableSpec};

/// 一条待补写的控制审计事件。
///
/// 主键**就是 `event_id`**：重启后重发同一 outbox 事件先按 event_id 去重，已写入就复
/// 用原事件位置（§8.5）。它只保存控制事件，不复制消息或工具结果。
#[derive(Debug, toasty::Model)]
#[table = "control_outbox"]
pub struct ControlOutboxRow {
    /// `event_id`，由写入控制事务的那一方固定。
    #[key]
    pub id: String,
    pub session_id: String,
    /// 事件 `type` 字符串。
    pub payload_type: String,
    /// `EventPayload` 的 `data` 部分（JSON）。
    pub payload: String,
    /// **原始发生时间**，不是补写时间（§8.3）。
    pub occurred_at: i64,
    pub delivered: bool,
    /// 补写后落在 JSONL 的哪个 seq；未交付时为 `0`。
    pub seq: i64,
    pub created_at: i64,
}

pub const SPEC: TableSpec = TableSpec {
    name: "control_outbox",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "control_outbox" ("id" TEXT NOT NULL, "session_id" TEXT NOT NULL, "payload_type" TEXT NOT NULL, "payload" TEXT NOT NULL, "occurred_at" BIGINT NOT NULL, "delivered" BOOLEAN NOT NULL, "seq" BIGINT NOT NULL, "created_at" BIGINT NOT NULL, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("session_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("payload_type", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("payload", "TEXT NOT NULL DEFAULT '{}'"),
    ColumnSpec::new("occurred_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("delivered", "BOOLEAN NOT NULL DEFAULT 0"),
    ColumnSpec::new("seq", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("created_at", "BIGINT NOT NULL DEFAULT 0"),
];
