//! `policy_grants`：有范围、来源、版本及有效条件的授权（§7.2、§8.2）。耐久表。

use super::{ColumnSpec, TableSpec};

/// 一条已生效的授权。
///
/// `scope` 是整个 `GrantScope` 的 JSON，而 `run_id` / `job_id` / `job_version` 另开列
/// ——按 Run 或按 Job 取有效授权是 SQL 里的筛选，不能指望从 JSON 里查（§8.2）。
#[derive(Debug, toasty::Model)]
#[table = "policy_grants"]
pub struct PolicyGrantRow {
    #[key]
    pub id: String,
    pub approval_id: String,
    /// `once` / `run` / `cron_job`。
    pub scope_kind: String,
    pub run_id: Option<String>,
    pub job_id: Option<String>,
    /// 绑定的 Job 版本；Job 改了，旧授权失效（§10）。
    pub job_version: i64,
    /// `GrantScope` 的 JSON。
    pub scope: String,
    pub granted_at: i64,
    /// `0` = 不过期。
    pub valid_until: i64,
    /// 一次性授权是否已被消费。**已经消费授权本身不是重试依据**（§7.4）。
    pub consumed: bool,
    pub reason: String,
}

pub const SPEC: TableSpec = TableSpec {
    name: "policy_grants",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "policy_grants" ("id" TEXT NOT NULL, "approval_id" TEXT NOT NULL, "scope_kind" TEXT NOT NULL, "run_id" TEXT, "job_id" TEXT, "job_version" BIGINT NOT NULL, "scope" TEXT NOT NULL, "granted_at" BIGINT NOT NULL, "valid_until" BIGINT NOT NULL, "consumed" BOOLEAN NOT NULL, "reason" TEXT NOT NULL, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("approval_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("scope_kind", "TEXT NOT NULL DEFAULT 'once'"),
    ColumnSpec::new("run_id", "TEXT"),
    ColumnSpec::new("job_id", "TEXT"),
    ColumnSpec::new("job_version", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("scope", "TEXT NOT NULL DEFAULT '{}'"),
    ColumnSpec::new("granted_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("valid_until", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("consumed", "BOOLEAN NOT NULL DEFAULT 0"),
    ColumnSpec::new("reason", "TEXT NOT NULL DEFAULT ''"),
];
