//! `cron_firings`：唯一触发记录与不可变触发快照（§10）。耐久表。

use super::{ColumnSpec, TableSpec};

/// 一次触发。
///
/// **主键是 `job_id + scheduled_at` 的确定性哈希**，不是一个新 UUID：§10 要求"同一计划
/// 时间不重复创建运行"，把这条唯一性放进主键，重复插入就是同一行，并发重复插入就是
/// 同一行上的写写冲突（`with_write_retry` 重跑后看见行已在，答 `false`）。
#[derive(Debug, toasty::Model)]
#[table = "cron_firings"]
pub struct CronFiringRow {
    #[key]
    pub id: String,
    pub job_id: String,
    pub job_version: i64,
    pub scheduled_at: i64,
    /// 不可变的触发快照：据此可以补完尚未写入的触发输入（§8.5）。
    pub prompt: String,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub created_at: i64,
}

pub const SPEC: TableSpec = TableSpec {
    name: "cron_firings",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "cron_firings" ("id" TEXT NOT NULL, "job_id" TEXT NOT NULL, "job_version" BIGINT NOT NULL, "scheduled_at" BIGINT NOT NULL, "prompt" TEXT NOT NULL, "session_id" TEXT, "run_id" TEXT, "created_at" BIGINT NOT NULL, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("job_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("job_version", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("scheduled_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("prompt", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("session_id", "TEXT"),
    ColumnSpec::new("run_id", "TEXT"),
    ColumnSpec::new("created_at", "BIGINT NOT NULL DEFAULT 0"),
];
