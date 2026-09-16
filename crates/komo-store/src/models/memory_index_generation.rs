//! `memory_index_generations`：向量空间指纹、构建状态、进度与生效代次（§9.5）。

use super::{ColumnSpec, TableSpec};

/// 一个索引代次。
///
/// 换模型 / 维度 / 预处理方式时创建新代次，**旧代次保持独立**：当前查询不能把新的
/// query vector 与旧向量比较（§9.5）。`active` 标的就是"当前查询用哪一代"。
#[derive(Debug, toasty::Model)]
#[table = "memory_index_generations"]
pub struct MemoryIndexGenerationRow {
    /// 代次 ID。
    #[key]
    pub id: String,
    /// `EmbeddingSpace` 的 JSON。没有配置向量模型时为 `NULL`。
    pub space: Option<String>,
    /// 空间指纹（`EmbeddingSpace::fingerprint`）。
    pub fingerprint: String,
    /// `IndexState`：unconfigured / building / ready / failed。
    pub state: String,
    pub dimensions: i64,
    pub indexed: i64,
    pub total: i64,
    /// `Vec<String>` 的 JSON。
    pub errors: String,
    pub active: bool,
    pub created_at: i64,
    pub activated_at: i64,
}

pub const SPEC: TableSpec = TableSpec {
    name: "memory_index_generations",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "memory_index_generations" ("id" TEXT NOT NULL, "space" TEXT, "fingerprint" TEXT NOT NULL, "state" TEXT NOT NULL, "dimensions" BIGINT NOT NULL, "indexed" BIGINT NOT NULL, "total" BIGINT NOT NULL, "errors" TEXT NOT NULL, "active" BOOLEAN NOT NULL, "created_at" BIGINT NOT NULL, "activated_at" BIGINT NOT NULL, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("space", "TEXT"),
    ColumnSpec::new("fingerprint", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("state", "TEXT NOT NULL DEFAULT 'unconfigured'"),
    ColumnSpec::new("dimensions", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("indexed", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("total", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("errors", "TEXT NOT NULL DEFAULT '[]'"),
    ColumnSpec::new("active", "BOOLEAN NOT NULL DEFAULT 0"),
    ColumnSpec::new("created_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("activated_at", "BIGINT NOT NULL DEFAULT 0"),
];
