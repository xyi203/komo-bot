//! `memory_vectors`：f32 BLOB + 维度 + 代次（§9.5）。可重建表。

use super::{ColumnSpec, TableSpec};

/// 一条记忆在某个索引代次的向量。
///
/// 按 `memory_id + revision + content_hash + generation` 定位（§9.5）——同维度不代表
/// 同一空间，所以代次必须进主键的构成。
#[derive(Debug, toasty::Model)]
#[table = "memory_vectors"]
pub struct MemoryVectorRow {
    /// 四元组的确定性哈希。
    #[key]
    pub id: String,
    pub memory_id: String,
    pub revision: i64,
    pub content_hash: String,
    pub generation: String,
    pub dimensions: i64,
    /// 小端序 f32 序列。
    pub vector: Vec<u8>,
    pub created_at: i64,
}

pub const SPEC: TableSpec = TableSpec {
    name: "memory_vectors",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "memory_vectors" ("id" TEXT NOT NULL, "memory_id" TEXT NOT NULL, "revision" BIGINT NOT NULL, "content_hash" TEXT NOT NULL, "generation" TEXT NOT NULL, "dimensions" BIGINT NOT NULL, "vector" BLOB NOT NULL, "created_at" BIGINT NOT NULL, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("memory_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("revision", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("content_hash", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("generation", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("dimensions", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("vector", "BLOB NOT NULL DEFAULT x''"),
    ColumnSpec::new("created_at", "BIGINT NOT NULL DEFAULT 0"),
];
