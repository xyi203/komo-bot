//! `memory_evidence`：来源事件或外部记录引用、提取与确认依据（§9.2）。耐久表。

use super::{ColumnSpec, TableSpec};

/// 一条证据。
///
/// 证据与内容版本**分别保留**（§9.2），所以它是自己的一张表而不是 `memory_items` 上的
/// 一个 JSON 列——一条记忆改了正文，旧证据说的还是旧那一版。
#[derive(Debug, toasty::Model)]
#[table = "memory_evidence"]
pub struct MemoryEvidenceRow {
    /// `memory_id` + `revision` + 序号的确定性哈希：同一条证据重复写入是幂等的。
    #[key]
    pub id: String,
    pub memory_id: String,
    pub revision: i64,
    pub ordinal: i64,
    /// `EvidenceRef` 的 JSON（Session + event_id / seq，或 Memos 实例 + 记录 ID）。
    pub reference: String,
    /// `Provenance`。
    pub provenance: String,
    pub observed_at: i64,
    pub extracted_from_run: Option<String>,
}

pub const SPEC: TableSpec = TableSpec {
    name: "memory_evidence",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "memory_evidence" ("id" TEXT NOT NULL, "memory_id" TEXT NOT NULL, "revision" BIGINT NOT NULL, "ordinal" BIGINT NOT NULL, "reference" TEXT NOT NULL, "provenance" TEXT NOT NULL, "observed_at" BIGINT NOT NULL, "extracted_from_run" TEXT, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("memory_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("revision", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("ordinal", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("reference", "TEXT NOT NULL DEFAULT '{}'"),
    ColumnSpec::new("provenance", "TEXT NOT NULL DEFAULT 'model_inference'"),
    ColumnSpec::new("observed_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("extracted_from_run", "TEXT"),
];
