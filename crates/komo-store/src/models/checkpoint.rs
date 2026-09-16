//! `checkpoints`：已覆盖的 seq、字节位置、格式版本与记忆版本引用（§8.2、§8.3）。

use super::{ColumnSpec, TableSpec};

/// 一个检查点。
///
/// 摘要正文**不在这里**——它是 JSONL 事件，检查点只引用范围（§8.3）。
#[derive(Debug, toasty::Model)]
#[table = "checkpoints"]
pub struct CheckpointRow {
    #[key]
    pub id: String,
    pub session_id: String,
    pub run_id: Option<String>,
    pub covers_from: i64,
    pub covers_to: i64,
    /// JSONL 字节位置，只是加速索引；校验不符就重新扫描（§8.3）。
    pub byte_offset: i64,
    pub format_version: i64,
    /// `Vec<MemoryUse>` 的 JSON（§9.7）。
    pub memories: String,
    pub retrieval_config_version: Option<String>,
    /// 执行游标：这个检查点之后从哪条事件继续。
    pub cursor: i64,
    pub created_at: i64,
}

pub const SPEC: TableSpec = TableSpec {
    name: "checkpoints",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "checkpoints" ("id" TEXT NOT NULL, "session_id" TEXT NOT NULL, "run_id" TEXT, "covers_from" BIGINT NOT NULL, "covers_to" BIGINT NOT NULL, "byte_offset" BIGINT NOT NULL, "format_version" BIGINT NOT NULL, "memories" TEXT NOT NULL, "retrieval_config_version" TEXT, "cursor" BIGINT NOT NULL, "created_at" BIGINT NOT NULL, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("session_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("run_id", "TEXT"),
    ColumnSpec::new("covers_from", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("covers_to", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("byte_offset", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("format_version", "BIGINT NOT NULL DEFAULT 1"),
    ColumnSpec::new("memories", "TEXT NOT NULL DEFAULT '[]'"),
    ColumnSpec::new("retrieval_config_version", "TEXT"),
    ColumnSpec::new("cursor", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("created_at", "BIGINT NOT NULL DEFAULT 0"),
];
