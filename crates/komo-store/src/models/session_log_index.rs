//! `session_log_index`：JSONL 每一行的位置与完整性摘要，**不存正文**（§8.2）。

use super::{ColumnSpec, TableSpec};

/// 一条事件在 JSONL 里的坐标。
///
/// 字节位置只是加速索引：校验不符时重新扫描并重建（§8.3），所以这张表是可重建表。
#[derive(Debug, toasty::Model)]
#[table = "session_log_index"]
pub struct SessionLogIndexRow {
    /// `event_id`。重复提交同一 ID 必须幂等（§8.3）。
    #[key]
    pub id: String,
    pub session_id: String,
    pub seq: i64,
    pub run_id: Option<String>,
    pub event_type: String,
    pub byte_offset: i64,
    /// 记录长度，**含结尾换行**。
    pub byte_len: i64,
    /// 这一行（不含换行）的 SHA-256。
    pub digest: String,
}

pub const SPEC: TableSpec = TableSpec {
    name: "session_log_index",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "session_log_index" ("id" TEXT NOT NULL, "session_id" TEXT NOT NULL, "seq" BIGINT NOT NULL, "run_id" TEXT, "event_type" TEXT NOT NULL, "byte_offset" BIGINT NOT NULL, "byte_len" BIGINT NOT NULL, "digest" TEXT NOT NULL, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("session_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("seq", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("run_id", "TEXT"),
    ColumnSpec::new("event_type", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("byte_offset", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("byte_len", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("digest", "TEXT NOT NULL DEFAULT ''"),
];
