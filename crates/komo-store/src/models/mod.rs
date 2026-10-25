//! toasty 模型定义；唯一展开 toasty 派生宏的地方（§13.4）。
//!
//! 一个模型一个文件，**它的 `*_TABLE_DDL` 常量就放在旁边**——`models::tests` 里有一个
//! 测试对每张表断言 toasty 为空库生成的 DDL 与常量字节相等（§8.2）。改了列却没改常量，
//! 那个测试就挂。
//!
//! 约定（写新模型前先读）：
//!
//! - 主键一律 `String` UUIDv7（`#[key] id: String`），不用 `#[auto]`——ID 要在写库之前
//!   就存在（跨库靠预分配 ID 与请求键幂等）。
//! - 时间一律 `i64` **unix 纳秒**（[`crate::db::to_ts`] / [`crate::db::from_ts`]）。可空的
//!   时间用哨兵 `0`（= 未设置）：NULL 在 SQL 的 `<=` 比较里会把整行筛掉。
//! - 结构化字段存 JSON 文本；**在 SQL 里被筛选或排序的维度另开一列**（`scope_kind`、
//!   `job_version`…），不要指望从 JSON 里查。
//! - **不加 `#[index]`**：索引由 [`crate::db::ensure_schema`] 用
//!   `CREATE INDEX IF NOT EXISTS` 统一建，新文件和旧文件走同一条路；让 toasty 建一半、
//!   自己建一半会分叉。

pub mod cron_firing;
pub mod cron_job;
pub mod delivery;
pub mod memory_evidence;
pub mod memory_index_generation;
pub mod memory_item;
pub mod memory_terms;
pub mod memory_vector;
pub mod session_kind;

pub use cron_firing::CronFiringRow;
pub use cron_job::CronJobRow;
pub use delivery::DeliveryRow;
pub use memory_evidence::MemoryEvidenceRow;
pub use memory_index_generation::MemoryIndexGenerationRow;
pub use memory_item::MemoryItemRow;
pub use memory_terms::MemoryTermsRow;
pub use memory_vector::MemoryVectorRow;
pub use session_kind::SessionKind;

/// 一列在 `ALTER TABLE ADD COLUMN` 里的写法，以及补完之后要跑的回填。
///
/// toasty 生成的 `CREATE TABLE` 不带 `DEFAULT`，而 SQLite 拒绝为已有数据的表加一个
/// 没有默认值的 `NOT NULL` 列——所以补列的子句必须在这里单独给出，**要么
/// `NOT NULL DEFAULT …`，要么可空**（§8.2）。
///
/// 默认值答不出的那一类列（"已有的行这一列该是什么"要看别的列）由 [`Self::backfill`]
/// 回答：回填与补列写在同一个地方，所以没有一条升级路径能漏掉它——`migrate_file`（文件
/// 库）与 `ensure_schema`（内存库）走的是同一份材料。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnSpec {
    pub name: &'static str,
    /// `ALTER TABLE "t" ADD COLUMN <name> <clause>` 里的 `<clause>`。
    pub clause: &'static str,
    /// 补完这一列之后在同一张表上跑一次的回填语句；`None` = 默认值就是答案。
    pub backfill: Option<&'static str>,
}

impl ColumnSpec {
    pub const fn new(name: &'static str, clause: &'static str) -> Self {
        Self {
            name,
            clause,
            backfill: None,
        }
    }

    /// 一列光有默认值不够、还要按别的列回填的写法。
    ///
    /// `backfill` 必须**幂等**：补列与回填没有事务包着（DDL 不进 `BEGIN CONCURRENT`），
    /// 半途失败重跑时要能接着跑。
    pub const fn with_backfill(
        name: &'static str,
        clause: &'static str,
        backfill: &'static str,
    ) -> Self {
        Self {
            name,
            clause,
            backfill: Some(backfill),
        }
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

/// `aux.db` 里的全部表，按建表顺序：cron、投递、记忆。
///
/// 会话 / run / 工具调用 / 检查点 / 控制 outbox 那几张旧表随旧存储删除（会话与提交在
/// `durable.db`，`komo-store::durable`）；老用户目录里的 `state.db` 不再被打开。
pub const TABLES: &[TableSpec] = &[
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
    r#"CREATE INDEX IF NOT EXISTS "deliveries_state" ON "deliveries" ("state")"#,
    r#"CREATE INDEX IF NOT EXISTS "cron_firings_job" ON "cron_firings" ("job_id", "scheduled_at")"#,
    r#"CREATE INDEX IF NOT EXISTS "memory_evidence_memory" ON "memory_evidence" ("memory_id")"#,
    r#"CREATE INDEX IF NOT EXISTS "memory_vectors_memory" ON "memory_vectors" ("memory_id", "generation")"#,
];

/// 交给 `Db::builder().models(..)` 的模型集合。
pub fn model_set() -> toasty::ModelSet {
    toasty::models!(
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
