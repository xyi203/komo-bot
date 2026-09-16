//! `cron_jobs`：定时任务定义、版本与授权（§10）。耐久表。

use super::{ColumnSpec, TableSpec};

/// 一个定时任务。
///
/// `next_run_at` 是调度器唯一找得到的槽位；`0` = 没有下一次（一次性已完成、暂停、或
/// 时区解析不出来——后者把原因写进 `last_error`，让它在清单里看得见而不是安静地再也
/// 不响，§10）。
#[derive(Debug, toasty::Model)]
#[table = "cron_jobs"]
pub struct CronJobRow {
    #[key]
    pub id: String,
    pub name: String,
    /// 定义版本。Job 改了，绑定它的授权失效（§7.2）。
    pub version: i64,
    /// `Trigger` 的 JSON（`cron` 表达式 + IANA 区名，或 `@at` 的瞬时）。
    pub trigger: String,
    pub prompt: String,
    pub workdir: Option<String>,
    /// `JobStatus`：active / paused / done。
    pub status: String,
    /// `OverlapPolicy`：skip / allow。
    pub overlap: String,
    /// 本 Job 的主模型覆盖（`ModelConfig` 的 JSON）。
    pub model: Option<String>,
    pub effort: Option<String>,
    /// `Vec<String>` 的 JSON。
    pub skills: String,
    /// `0` = 不限。
    pub max_rounds: i64,
    /// `0` = 没有下一个槽位。
    pub next_run_at: i64,
    /// 触发 / 配置层面的问题。执行失败记在 firing 上，不记这里。
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub const SPEC: TableSpec = TableSpec {
    name: "cron_jobs",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "cron_jobs" ("id" TEXT NOT NULL, "name" TEXT NOT NULL, "version" BIGINT NOT NULL, "trigger" TEXT NOT NULL, "prompt" TEXT NOT NULL, "workdir" TEXT, "status" TEXT NOT NULL, "overlap" TEXT NOT NULL, "model" TEXT, "effort" TEXT, "skills" TEXT NOT NULL, "max_rounds" BIGINT NOT NULL, "next_run_at" BIGINT NOT NULL, "last_error" TEXT, "created_at" BIGINT NOT NULL, "updated_at" BIGINT NOT NULL, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("name", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("version", "BIGINT NOT NULL DEFAULT 1"),
    ColumnSpec::new("trigger", "TEXT NOT NULL DEFAULT '{}'"),
    ColumnSpec::new("prompt", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("workdir", "TEXT"),
    ColumnSpec::new("status", "TEXT NOT NULL DEFAULT 'active'"),
    ColumnSpec::new("overlap", "TEXT NOT NULL DEFAULT 'skip'"),
    ColumnSpec::new("model", "TEXT"),
    ColumnSpec::new("effort", "TEXT"),
    ColumnSpec::new("skills", "TEXT NOT NULL DEFAULT '[]'"),
    ColumnSpec::new("max_rounds", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("next_run_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("last_error", "TEXT"),
    ColumnSpec::new("created_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("updated_at", "BIGINT NOT NULL DEFAULT 0"),
];
