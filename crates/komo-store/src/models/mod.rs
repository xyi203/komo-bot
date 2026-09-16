//! toasty 模型定义；唯一展开 toasty 派生宏的地方（§13.4）。
//!
//! 一个模型一个文件，**它的 `*_TABLE_DDL` 常量就放在旁边**——`models::tests` 里有一个
//! 测试对每张表断言 toasty 为空库生成的 DDL 与常量字节相等（§8.2）。改了列却没改常量，
//! 那个测试就挂。
//!
//! 约定（写新模型前先读）：
//!
//! - 主键一律 `String` UUIDv7（`#[key] id: String`），不用 `#[auto]`——ID 要在写库之前
//!   就存在（JSONL 先写、事件里带 ID、跨进程恢复按 ID 对账）。
//! - 时间一律 `i64` **unix 纳秒**（[`crate::db::to_ts`] / [`crate::db::from_ts`]）。可空的
//!   时间用哨兵 `0`（= 未设置），因为 §8.7 的领取 SQL 要对 `next_retry_at` 直接做
//!   `<= ?1` 比较，NULL 在那里会把整行筛掉。
//! - 结构化字段存 JSON 文本；**在 SQL 里被筛选或排序的维度另开一列**（`scope_kind`、
//!   `job_version`…），不要指望从 JSON 里查。
//! - **不加 `#[index]`**：索引由 [`crate::db::ensure_schema`] 用
//!   `CREATE INDEX IF NOT EXISTS` 统一建，新文件和旧文件走同一条路；让 toasty 建一半、
//!   自己建一半会分叉。

pub mod approval_request;
pub mod checkpoint;
pub mod control_outbox;
pub mod cron_firing;
pub mod cron_job;
pub mod delivery;
pub mod memory_evidence;
pub mod memory_index_generation;
pub mod memory_item;
pub mod memory_terms;
pub mod memory_vector;
pub mod policy_grant;
pub mod run;
pub mod session;
pub mod session_log_index;
pub mod tool_attempt;
pub mod tool_call;

pub use approval_request::ApprovalRequestRow;
pub use checkpoint::CheckpointRow;
pub use control_outbox::ControlOutboxRow;
pub use cron_firing::CronFiringRow;
pub use cron_job::CronJobRow;
pub use delivery::DeliveryRow;
pub use memory_evidence::MemoryEvidenceRow;
pub use memory_index_generation::MemoryIndexGenerationRow;
pub use memory_item::MemoryItemRow;
pub use memory_terms::MemoryTermsRow;
pub use memory_vector::MemoryVectorRow;
pub use policy_grant::PolicyGrantRow;
pub use run::RunRow;
pub use session::SessionRow;
pub use session_log_index::SessionLogIndexRow;
pub use tool_attempt::ToolAttemptRow;
pub use tool_call::ToolCallRow;

/// 一列在 `ALTER TABLE ADD COLUMN` 里的写法。
///
/// toasty 生成的 `CREATE TABLE` 不带 `DEFAULT`，而 SQLite 拒绝为已有数据的表加一个
/// 没有默认值的 `NOT NULL` 列——所以补列的子句必须在这里单独给出，**要么
/// `NOT NULL DEFAULT …`，要么可空**（§8.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnSpec {
    pub name: &'static str,
    /// `ALTER TABLE "t" ADD COLUMN <name> <clause>` 里的 `<clause>`。
    pub clause: &'static str,
}

impl ColumnSpec {
    pub const fn new(name: &'static str, clause: &'static str) -> Self {
        Self { name, clause }
    }
}

/// 一张表的 schema 演进材料。
#[derive(Debug, Clone, Copy)]
pub struct TableSpec {
    pub name: &'static str,
    /// 与 toasty 对空库生成的 DDL **字节相等**。
    pub ddl: &'static str,
    pub columns: &'static [ColumnSpec],
}

/// §8.2 表清单里的全部 17 张表，按建表顺序。
pub const TABLES: &[TableSpec] = &[
    session::SPEC,
    run::SPEC,
    session_log_index::SPEC,
    tool_call::SPEC,
    tool_attempt::SPEC,
    checkpoint::SPEC,
    approval_request::SPEC,
    policy_grant::SPEC,
    control_outbox::SPEC,
    delivery::SPEC,
    cron_job::SPEC,
    cron_firing::SPEC,
    memory_item::SPEC,
    memory_evidence::SPEC,
    memory_terms::SPEC,
    memory_vector::SPEC,
    memory_index_generation::SPEC,
];

/// 索引由 store 自己建（见模块头），新文件与旧文件走同一条路。
pub const INDEXES: &[&str] = &[
    r#"CREATE INDEX IF NOT EXISTS "runs_session" ON "runs" ("session_id")"#,
    r#"CREATE INDEX IF NOT EXISTS "runs_claimable" ON "runs" ("status", "next_retry_at")"#,
    r#"CREATE INDEX IF NOT EXISTS "log_index_session_seq" ON "session_log_index" ("session_id", "seq")"#,
    r#"CREATE INDEX IF NOT EXISTS "tool_calls_run" ON "tool_calls" ("run_id")"#,
    r#"CREATE INDEX IF NOT EXISTS "tool_attempts_call" ON "tool_attempts" ("call_id")"#,
    r#"CREATE INDEX IF NOT EXISTS "checkpoints_session" ON "checkpoints" ("session_id", "covers_to")"#,
    r#"CREATE INDEX IF NOT EXISTS "approvals_pending" ON "approval_requests" ("decided", "short_id")"#,
    r#"CREATE INDEX IF NOT EXISTS "grants_run" ON "policy_grants" ("run_id")"#,
    r#"CREATE INDEX IF NOT EXISTS "grants_job" ON "policy_grants" ("job_id", "job_version")"#,
    r#"CREATE INDEX IF NOT EXISTS "outbox_undelivered" ON "control_outbox" ("delivered")"#,
    r#"CREATE INDEX IF NOT EXISTS "deliveries_state" ON "deliveries" ("state")"#,
    r#"CREATE INDEX IF NOT EXISTS "deliveries_approval" ON "deliveries" ("approval_id")"#,
    r#"CREATE INDEX IF NOT EXISTS "cron_firings_job" ON "cron_firings" ("job_id", "scheduled_at")"#,
    r#"CREATE INDEX IF NOT EXISTS "memory_evidence_memory" ON "memory_evidence" ("memory_id")"#,
    r#"CREATE INDEX IF NOT EXISTS "memory_vectors_memory" ON "memory_vectors" ("memory_id", "generation")"#,
];

/// 交给 `Db::builder().models(..)` 的模型集合。
pub fn model_set() -> toasty::ModelSet {
    toasty::models!(
        SessionRow,
        RunRow,
        SessionLogIndexRow,
        ToolCallRow,
        ToolAttemptRow,
        CheckpointRow,
        ApprovalRequestRow,
        PolicyGrantRow,
        ControlOutboxRow,
        DeliveryRow,
        CronJobRow,
        CronFiringRow,
        MemoryItemRow,
        MemoryEvidenceRow,
        MemoryTermsRow,
        MemoryVectorRow,
        MemoryIndexGenerationRow,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// §8.2：「一个测试对每张表断言 toasty 为空库生成的 DDL 与常量**字节相等**——模型改
    /// 了列却没改常量，测试挂。」
    #[tokio::test]
    async fn every_table_ddl_matches_its_constant_byte_for_byte() {
        let db = crate::db::Db::open_memory().await.expect("打开内存库");
        let actual: std::collections::BTreeMap<String, String> = db
            .table_ddl()
            .await
            .expect("读 sqlite_master")
            .into_iter()
            .collect();

        assert_eq!(
            actual.len(),
            TABLES.len(),
            "建出来的表数与 TABLES 不一致：{:?}",
            actual.keys().collect::<Vec<_>>()
        );

        for table in TABLES {
            let generated = actual
                .get(table.name)
                .unwrap_or_else(|| panic!("toasty 没有建出表 {}", table.name));
            assert_eq!(
                generated.as_str(),
                table.ddl,
                "{} 的 DDL 与常量不一致——改了模型就要改旁边的常量",
                table.name
            );
        }
    }

    /// 常量里写着哪些列，`ColumnSpec` 列表就要有哪些列——否则 `ensure_schema` 补不出
    /// 一个旧库缺的列，而 DDL 对齐测试又看不出来。
    #[test]
    fn every_column_in_the_ddl_has_an_alter_clause() {
        for table in TABLES {
            let in_ddl: BTreeSet<&str> = columns_in_ddl(table.ddl);
            let in_spec: BTreeSet<&str> = table.columns.iter().map(|c| c.name).collect();
            assert_eq!(in_ddl, in_spec, "{} 的列清单对不上", table.name);
        }
    }

    /// 新列必须 `NOT NULL DEFAULT …` 或可空（§8.2）——否则 SQLite 拒绝给有数据的表加列。
    #[test]
    fn every_alter_clause_is_addable_to_a_table_that_already_has_rows() {
        for table in TABLES {
            for column in table.columns {
                let clause = column.clause;
                let addable = !clause.contains("NOT NULL") || clause.contains("DEFAULT");
                assert!(
                    addable,
                    "{}.{} 的补列子句既 NOT NULL 又没有 DEFAULT：{clause}",
                    table.name, column.name
                );
            }
        }
    }

    #[test]
    fn table_names_are_unique() {
        let names: BTreeSet<&str> = TABLES.iter().map(|t| t.name).collect();
        assert_eq!(names.len(), TABLES.len());
    }

    fn columns_in_ddl(ddl: &str) -> BTreeSet<&str> {
        // `CREATE TABLE "t" ("a" TEXT NOT NULL, …, PRIMARY KEY ("a"))`
        let body = ddl.split_once('(').expect("DDL 有列表").1;
        let mut out = BTreeSet::new();
        for part in body.split(", ") {
            let part = part.trim();
            if part.starts_with("PRIMARY KEY") {
                continue;
            }
            if let Some(rest) = part.strip_prefix('"')
                && let Some((name, _)) = rest.split_once('"')
            {
                out.insert(name);
            }
        }
        out
    }
}
