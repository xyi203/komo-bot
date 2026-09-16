//! `tool_attempts`：一次调用实际执行过的各次尝试（§8.1、§8.6）。

use super::{ColumnSpec, TableSpec};

/// 一次实际执行尝试。
///
/// `executor` + `process` 是 §8.7 的"不能仅凭一个 PID 判断是否为原进程"：启动身份和
/// 进程身份一起记，核对时两者都要对得上。
#[derive(Debug, toasty::Model)]
#[table = "tool_attempts"]
pub struct ToolAttemptRow {
    #[key]
    pub id: String,
    pub call_id: String,
    pub run_id: String,
    pub session_id: String,
    /// 第几次尝试，从 1 开始。
    pub ordinal: i64,
    /// 本次启动身份（执行实例 ID）。
    pub executor: Option<String>,
    /// 进程身份：`{pid}:{启动时间}`，不是裸 PID（§8.7）。
    pub process: Option<String>,
    /// `AttemptState`。
    pub state: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub started_event: Option<String>,
    pub result_event: Option<String>,
}

pub const SPEC: TableSpec = TableSpec {
    name: "tool_attempts",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "tool_attempts" ("id" TEXT NOT NULL, "call_id" TEXT NOT NULL, "run_id" TEXT NOT NULL, "session_id" TEXT NOT NULL, "ordinal" BIGINT NOT NULL, "executor" TEXT, "process" TEXT, "state" TEXT NOT NULL, "started_at" BIGINT NOT NULL, "ended_at" BIGINT NOT NULL, "started_event" TEXT, "result_event" TEXT, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("call_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("run_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("session_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("ordinal", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("executor", "TEXT"),
    ColumnSpec::new("process", "TEXT"),
    ColumnSpec::new("state", "TEXT NOT NULL DEFAULT 'started'"),
    ColumnSpec::new("started_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("ended_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("started_event", "TEXT"),
    ColumnSpec::new("result_event", "TEXT"),
];
