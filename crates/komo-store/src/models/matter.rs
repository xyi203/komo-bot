//! `matters`：目标与领域（`docs/goals.md` §3）。耐久表。

use super::{ColumnSpec, TableSpec};

/// 一个目标或领域。两种一张表，`kind` 区分；种类专属的列在另一种上恒为 NULL。
#[derive(Debug, toasty::Model)]
#[table = "matters"]
pub struct MatterRow {
    #[key]
    pub id: String,
    pub revision: i64,
    /// `MatterKind`：goal / area。
    pub kind: String,
    pub title: String,
    /// `MatterState`：active / waiting / parked / done / dropped / retired。
    pub state: String,
    /// 只给 goal：下一步。
    pub next: Option<String>,
    /// 只给 goal：在等谁、等什么。
    pub waiting_for: Option<String>,
    /// 只给 goal：截止日期，`YYYY-MM-DD`。
    pub due: Option<String>,
    /// 只给 area：要维持的标准。
    pub standard: Option<String>,
    /// `Vec<Evidence>` 的 JSON，追加不改。
    pub evidence: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// `0` = 未结案。
    pub closed_at: i64,
    /// `Vec<MatterTrigger>` 的 JSON（P1e，附加列）。
    pub triggers: String,
}

pub const SPEC: TableSpec = TableSpec {
    name: "matters",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "matters" ("id" TEXT NOT NULL, "revision" BIGINT NOT NULL, "kind" TEXT NOT NULL, "title" TEXT NOT NULL, "state" TEXT NOT NULL, "next" TEXT, "waiting_for" TEXT, "due" TEXT, "standard" TEXT, "evidence" TEXT NOT NULL, "created_at" BIGINT NOT NULL, "updated_at" BIGINT NOT NULL, "closed_at" BIGINT NOT NULL, "triggers" TEXT NOT NULL, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("revision", "BIGINT NOT NULL DEFAULT 1"),
    ColumnSpec::new("kind", "TEXT NOT NULL DEFAULT 'goal'"),
    ColumnSpec::new("title", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("state", "TEXT NOT NULL DEFAULT 'active'"),
    ColumnSpec::new("next", "TEXT"),
    ColumnSpec::new("waiting_for", "TEXT"),
    ColumnSpec::new("due", "TEXT"),
    ColumnSpec::new("standard", "TEXT"),
    ColumnSpec::new("evidence", "TEXT NOT NULL DEFAULT '[]'"),
    ColumnSpec::new("created_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("updated_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("closed_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("triggers", "TEXT NOT NULL DEFAULT '[]'"),
];
