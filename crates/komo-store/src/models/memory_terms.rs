//! `memory_terms`：索引时分词的关键词列（§9.4）。可重建表。

use super::{ColumnSpec, TableSpec};

/// 一条记忆的关键词串。
///
/// Turso 的 MVCC 下建不出 FTS 索引（§8.2 实测），所以分词挪到**索引时**：`terms` 是
/// 用空格连接、**首尾也带空格**的 token 串，查询时每个 token 一个
/// `instr(terms, ' tok ') > 0`，命中数按 IDF 加权（§9.4）。
#[derive(Debug, toasty::Model)]
#[table = "memory_terms"]
pub struct MemoryTermsRow {
    /// 就是 `memory_id`——一条记忆只有一行当前关键词。
    #[key]
    pub id: String,
    pub memory_id: String,
    pub revision: i64,
    /// 首尾带空格的 token 串。
    pub terms: String,
    pub term_count: i64,
}

pub const SPEC: TableSpec = TableSpec {
    name: "memory_terms",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "memory_terms" ("id" TEXT NOT NULL, "memory_id" TEXT NOT NULL, "revision" BIGINT NOT NULL, "terms" TEXT NOT NULL, "term_count" BIGINT NOT NULL, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("memory_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("revision", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("terms", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("term_count", "BIGINT NOT NULL DEFAULT 0"),
];
