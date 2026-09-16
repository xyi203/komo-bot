//! `runs`：一次用户输入或一次触发引起的持久任务（§8.1、§8.2、§8.7）。

use super::{ColumnSpec, TableSpec};

/// 一个 Run 的调度状态。
///
/// `claimed_by` / `claim_generation` 是 §8.7 的领取代次围栏：领取是
/// `claimed_by IS NULL` 上的条件更新，之后这个执行者的每一条状态提交都带
/// `AND claim_generation = ?`，`rows affected == 0` 就是自己已成旧代次。
///
/// `next_retry_at` 是 `NOT NULL DEFAULT 0` 而不是可空：§8.7 的候选查询对它直接做
/// `<= ?1`，NULL 会把"没有退避、现在就能跑"的行整行筛掉。
#[derive(Debug, toasty::Model)]
#[table = "runs"]
pub struct RunRow {
    #[key]
    pub id: String,
    pub session_id: String,
    /// 幂等键。同一键重发返回原 Run，内容哈希不同则拒绝（§8.5）。
    pub request_key: String,
    pub input_hash: String,
    /// 承载输入的事件（`run.accepted`）。
    pub input_event: Option<String>,
    /// 承载终态的事件。
    pub final_event: Option<String>,
    pub status: String,
    /// `PlanSource` 的 JSON。恢复后来源不变——Cron 恢复后仍是 Cron（§8.8）。
    pub source: String,
    /// 来源会话的 `{platform}:{chat_id}`。
    pub peer: Option<String>,
    /// 本次启动身份；`NULL` = 没人领。
    pub claimed_by: Option<String>,
    pub claim_generation: i64,
    pub claimed_at: i64,
    pub next_retry_at: i64,
    pub retry_attempts: i64,
    pub rounds: i64,
    pub max_rounds: i64,
    /// 审批有效期；`0` = 不限。
    pub valid_until: i64,
    /// 本次 Run 固定的 `ModelConfig` 快照，只记身份不记凭证。
    pub model_snapshot: String,
    pub effort: Option<String>,
    /// 这个 Run 上的范围授权 ID 列表（JSON）。权威仍在 `policy_grants`。
    pub grants: String,
    /// `MemoryWork`：pending / processing / done / error（§9.3）。
    pub memory_work: String,
    /// 记忆处理游标（已处理到的 seq）。
    pub memory_cursor: i64,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub ended_at: i64,
}

pub const SPEC: TableSpec = TableSpec {
    name: "runs",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "runs" ("id" TEXT NOT NULL, "session_id" TEXT NOT NULL, "request_key" TEXT NOT NULL, "input_hash" TEXT NOT NULL, "input_event" TEXT, "final_event" TEXT, "status" TEXT NOT NULL, "source" TEXT NOT NULL, "peer" TEXT, "claimed_by" TEXT, "claim_generation" BIGINT NOT NULL, "claimed_at" BIGINT NOT NULL, "next_retry_at" BIGINT NOT NULL, "retry_attempts" BIGINT NOT NULL, "rounds" BIGINT NOT NULL, "max_rounds" BIGINT NOT NULL, "valid_until" BIGINT NOT NULL, "model_snapshot" TEXT NOT NULL, "effort" TEXT, "grants" TEXT NOT NULL, "memory_work" TEXT NOT NULL, "memory_cursor" BIGINT NOT NULL, "last_error" TEXT, "created_at" BIGINT NOT NULL, "updated_at" BIGINT NOT NULL, "ended_at" BIGINT NOT NULL, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("session_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("request_key", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("input_hash", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("input_event", "TEXT"),
    ColumnSpec::new("final_event", "TEXT"),
    ColumnSpec::new("status", "TEXT NOT NULL DEFAULT 'ingesting'"),
    ColumnSpec::new("source", "TEXT NOT NULL DEFAULT '{}'"),
    ColumnSpec::new("peer", "TEXT"),
    ColumnSpec::new("claimed_by", "TEXT"),
    ColumnSpec::new("claim_generation", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("claimed_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("next_retry_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("retry_attempts", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("rounds", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("max_rounds", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("valid_until", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("model_snapshot", "TEXT NOT NULL DEFAULT '{}'"),
    ColumnSpec::new("effort", "TEXT"),
    ColumnSpec::new("grants", "TEXT NOT NULL DEFAULT '[]'"),
    ColumnSpec::new("memory_work", "TEXT NOT NULL DEFAULT 'pending'"),
    ColumnSpec::new("memory_cursor", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("last_error", "TEXT"),
    ColumnSpec::new("created_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("updated_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("ended_at", "BIGINT NOT NULL DEFAULT 0"),
];
