//! `tool_calls`：一次有独立执行状态的逻辑调用（§8.1、§8.2）。

use super::{ColumnSpec, TableSpec};

/// 一个逻辑调用。重试沿用同一行，新增一条 `tool_attempts`（§8.6）。
#[derive(Debug, toasty::Model)]
#[table = "tool_calls"]
pub struct ToolCallRow {
    #[key]
    pub id: String,
    pub session_id: String,
    pub run_id: String,
    pub tool: String,
    /// provider 自己的 call_id，回放时按它配对；**重复的 provider ID 不能误命中其他
    /// 轮次**，所以轮次另存一列（§8.1）。
    pub provider_call_id: String,
    pub round: i64,
    /// `ToolCallState`。
    pub state: String,
    /// 承载参数的事件（`message.assistant`）。
    pub args_event: Option<String>,
    /// 承载计划的事件（`tool.planned`）。
    pub plan_event: Option<String>,
    /// 承载结果的事件（`tool.result`）。
    pub result_event: Option<String>,
    pub plan_hash: Option<String>,
    /// `RecoveryMode` 的 JSON——由 `prepare` 填写，模型不能随意填"可重试"（§8.6）。
    pub recovery: String,
    /// 外部幂等键（`RecoveryMode::IdempotencyKey`）。
    pub idempotency_key: Option<String>,
    pub attempts: i64,
    /// `OutputRef` 的 JSON。
    pub output_ref: Option<String>,
    /// 最多 1 KiB 的预览（§8.3）。
    pub preview: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub const SPEC: TableSpec = TableSpec {
    name: "tool_calls",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "tool_calls" ("id" TEXT NOT NULL, "session_id" TEXT NOT NULL, "run_id" TEXT NOT NULL, "tool" TEXT NOT NULL, "provider_call_id" TEXT NOT NULL, "round" BIGINT NOT NULL, "state" TEXT NOT NULL, "args_event" TEXT, "plan_event" TEXT, "result_event" TEXT, "plan_hash" TEXT, "recovery" TEXT NOT NULL, "idempotency_key" TEXT, "attempts" BIGINT NOT NULL, "output_ref" TEXT, "preview" TEXT, "created_at" BIGINT NOT NULL, "updated_at" BIGINT NOT NULL, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("session_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("run_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("tool", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("provider_call_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("round", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("state", "TEXT NOT NULL DEFAULT 'planned'"),
    ColumnSpec::new("args_event", "TEXT"),
    ColumnSpec::new("plan_event", "TEXT"),
    ColumnSpec::new("result_event", "TEXT"),
    ColumnSpec::new("plan_hash", "TEXT"),
    ColumnSpec::new("recovery", "TEXT NOT NULL DEFAULT '{}'"),
    ColumnSpec::new("idempotency_key", "TEXT"),
    ColumnSpec::new("attempts", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("output_ref", "TEXT"),
    ColumnSpec::new("preview", "TEXT"),
    ColumnSpec::new("created_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("updated_at", "BIGINT NOT NULL DEFAULT 0"),
];
