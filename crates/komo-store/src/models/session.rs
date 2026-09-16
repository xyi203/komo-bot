//! `sessions`：连续对话、工作目录与上下文的载体（§8.1、§8.2）。

use super::{ColumnSpec, TableSpec};

/// 一个 Session 的元数据。
///
/// `applied_seq` / `applied_bytes` 是**已应用的连续前缀**：它只能推进到已经校验过的
/// 事件为止（§8.5），所以它落后于 JSONL 是正常的，超前则是损坏。
#[derive(Debug, toasty::Model)]
#[table = "sessions"]
pub struct SessionRow {
    #[key]
    pub id: String,
    pub title: String,
    /// 来源：`{platform}:{chat_id}`，或 `api` / `cron:{job}`。
    pub origin: String,
    pub workdir: Option<String>,
    pub current_run: Option<String>,
    /// 相对数据目录的 JSONL 路径。
    pub jsonl_path: String,
    pub applied_seq: i64,
    pub applied_bytes: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

pub const SPEC: TableSpec = TableSpec {
    name: "sessions",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "sessions" ("id" TEXT NOT NULL, "title" TEXT NOT NULL, "origin" TEXT NOT NULL, "workdir" TEXT, "current_run" TEXT, "jsonl_path" TEXT NOT NULL, "applied_seq" BIGINT NOT NULL, "applied_bytes" BIGINT NOT NULL, "created_at" BIGINT NOT NULL, "updated_at" BIGINT NOT NULL, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("title", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("origin", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("workdir", "TEXT"),
    ColumnSpec::new("current_run", "TEXT"),
    ColumnSpec::new("jsonl_path", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("applied_seq", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("applied_bytes", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("created_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("updated_at", "BIGINT NOT NULL DEFAULT 0"),
];
