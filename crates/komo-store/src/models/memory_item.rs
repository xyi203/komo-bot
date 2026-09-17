//! `memory_items`：自动记忆的正文、作用域、确认状态与生命周期（§9.2）。耐久表。

use super::{ColumnSpec, TableSpec};

/// 一条自动记忆。
///
/// `confirmation` 与 `provenance` 是**两列**，不是一列：把"模型从用户原话整理"写成
/// "用户确认了模型摘要"正是这组字段存在的全部理由（§9.2）。
#[derive(Debug, toasty::Model)]
#[table = "memory_items"]
pub struct MemoryItemRow {
    #[key]
    pub id: String,
    /// 递增内容版本；修改产生新版本。confirm / forget 携带预期 revision（§9.6）。
    pub revision: i64,
    pub content: String,
    /// 正文哈希——向量入库前要确认正文未变（§9.5）。
    pub content_hash: String,
    /// `MemoryKind`。
    pub kind: String,
    /// `personal` / `project` / `environment`，另开一列供 SQL 筛选。
    pub scope_kind: String,
    /// `MemoryScope` 的 JSON。
    pub scope: String,
    /// `Provenance`。
    pub provenance: String,
    /// `Confirmation`。
    pub confirmation: String,
    /// `MemoryState`。
    pub state: String,
    /// 事实的**观察**时间，区别于入库时间。
    pub observed_at: i64,
    /// `0` = 不过期。
    pub valid_until: i64,
    pub created_at: i64,
    pub updated_at: i64,
    /// `ExtractionMetadata` 的 JSON。
    pub extraction: String,
    /// 使用次数。**只度量使用，不增加真实性**（§9.2）。
    pub usage_count: i64,
    pub last_used_at: i64,
    /// `SupersededRef` 的 JSON；`NULL` = 这条不取代任何东西（§9.6）。
    pub supersedes: Option<String>,
}

pub const SPEC: TableSpec = TableSpec {
    name: "memory_items",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "memory_items" ("id" TEXT NOT NULL, "revision" BIGINT NOT NULL, "content" TEXT NOT NULL, "content_hash" TEXT NOT NULL, "kind" TEXT NOT NULL, "scope_kind" TEXT NOT NULL, "scope" TEXT NOT NULL, "provenance" TEXT NOT NULL, "confirmation" TEXT NOT NULL, "state" TEXT NOT NULL, "observed_at" BIGINT NOT NULL, "valid_until" BIGINT NOT NULL, "created_at" BIGINT NOT NULL, "updated_at" BIGINT NOT NULL, "extraction" TEXT NOT NULL, "usage_count" BIGINT NOT NULL, "last_used_at" BIGINT NOT NULL, "supersedes" TEXT, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("revision", "BIGINT NOT NULL DEFAULT 1"),
    ColumnSpec::new("content", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("content_hash", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("kind", "TEXT NOT NULL DEFAULT 'fact'"),
    ColumnSpec::new("scope_kind", "TEXT NOT NULL DEFAULT 'personal'"),
    ColumnSpec::new("scope", "TEXT NOT NULL DEFAULT '{}'"),
    ColumnSpec::new("provenance", "TEXT NOT NULL DEFAULT 'model_inference'"),
    ColumnSpec::new("confirmation", "TEXT NOT NULL DEFAULT 'unconfirmed'"),
    ColumnSpec::new("state", "TEXT NOT NULL DEFAULT 'candidate'"),
    ColumnSpec::new("observed_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("valid_until", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("created_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("updated_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("extraction", "TEXT NOT NULL DEFAULT '{}'"),
    ColumnSpec::new("usage_count", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("last_used_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("supersedes", "TEXT"),
];
