//! `runs`：一次用户输入或一次触发引起的持久任务（§8.1、§8.2、§8.7）。

use super::{ColumnSpec, TableSpec};

/// 一个 Run 的调度状态。
///
/// **状态是两个维度**（§8.10 的加列规则）：`state` 一个词（`accepted` / `queued` /
/// `running` / `waiting` / `completed` / `failed` / `cancelled` / `abandoned`），
/// `waiting` 时由 `wait_kind` 说清**在等谁**（`approval` / `retry` / `intervention` /
/// `dependency`），`wait_ref` 指到具体对象（审批 ID、intervention 句柄、前置 Run ID，
/// `retry` 时是原因），`wake_at` 只有 `retry` 用。
///
/// 旧列 `status` / `next_retry_at` **退役但仍存在**（§8.2 只允许加列）：新写入一律给空值
/// （`''` / `0`），读的一方不再看它们。两列留在那里只为了让旧库能读、旧备份能对。
///
/// `claimed_by` / `claim_generation` 是领取代次围栏：领取是 `claimed_by IS NULL` 上的条件
/// 更新，之后这个执行者的每一条状态提交都带 `AND claim_generation = ?`，
/// `rows affected == 0` 就是自己已成旧代次。
///
/// `lease_until` 是心跳租约（unix 纳秒，`0` = 没租约，视为立即过期）。**租约过期只是
/// 信号，不是抢走一条活着的长调用的许可**：回收前还要过存活判定。
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
    /// 承载输入那条事件的 `seq`（§8.3：Session 内按追加顺序严格递增）。
    ///
    /// **同 Session 内的次序权威是它，不是 Run ID 的字典序**：UUIDv7 同一纳秒内的低位是
    /// 随机的，两条几乎同时受理的输入按 id 比会排反。`0` = 未知（这次改造之前受理的行），
    /// 那种行的次序退化到按 id 比。
    pub input_seq: i64,
    /// 承载终态的事件。
    pub final_event: Option<String>,
    /// 派它的那条 Run（§8.4 的委派子 Run）。`None` = 这是一条顶层 Run。
    ///
    /// **单独开一列而不是从 `delegate` 的 JSON 里取**：领取与依赖放行要在 SQL 里判"拦住
    /// 我的那条 Run 是不是我的父"（[`crate::repos::queue`] 的两句 `NOT EXISTS`），而
    /// "在 SQL 里被筛选的维度另开一列"是这张表的加列规则（见 [`super`]）——从 JSON 里
    /// 查 = 每次领取都解析一遍计划。这一列是**判据**，`delegate` 那一列是**正文**。
    pub parent_run_id: Option<String>,
    /// 受理这条 Run 的 [`komo_kernel::types::delegate::DelegateSpec`] JSON，**父侧那一份
    /// 计划留在子 Run 行上的副本**（授权与审批绑定的是计划，计划里就有它）。
    ///
    /// 它读不出来时**不猜**：当作"没有契约"（见 [`crate::repos::runs::delegate_of`]），
    /// 旧行与损坏行都要能读——子 Run 仍然可以跑，只是父侧复验时按自由文本处理。
    pub delegate: Option<String>,
    /// **退役**：读了不再用它，写入给空值。
    pub status: String,
    /// 调度状态：`accepted` / `queued` / `running` / `waiting` / `completed` / `failed` /
    /// `cancelled` / `abandoned`。
    pub state: String,
    /// `waiting` 时在等谁：`approval` / `retry` / `intervention` / `dependency`。
    pub wait_kind: Option<String>,
    /// `wait_kind` 指到的对象（审批 ID / intervention 句柄 / 前置 Run ID；`retry` 是原因）。
    pub wait_ref: Option<String>,
    /// `retry` 的 not_before；其他等待是 `0`。
    pub wake_at: i64,
    /// 心跳租约：`running` 的持有者在跑的时候续它。`0` = 没租约。
    pub lease_until: i64,
    /// `PlanSource` 的 JSON。恢复后来源不变——Cron 恢复后仍是 Cron（§8.8）。
    pub source: String,
    /// 来源会话的 `{platform}:{chat_id}`。
    pub peer: Option<String>,
    /// 本次启动身份；`NULL` = 没人领。
    pub claimed_by: Option<String>,
    pub claim_generation: i64,
    pub claimed_at: i64,
    /// **退役**：退避看 `wake_at`。
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

pub const DDL: &str = r#"CREATE TABLE "runs" ("id" TEXT NOT NULL, "session_id" TEXT NOT NULL, "request_key" TEXT NOT NULL, "input_hash" TEXT NOT NULL, "input_event" TEXT, "input_seq" BIGINT NOT NULL, "final_event" TEXT, "parent_run_id" TEXT, "delegate" TEXT, "status" TEXT NOT NULL, "state" TEXT NOT NULL, "wait_kind" TEXT, "wait_ref" TEXT, "wake_at" BIGINT NOT NULL, "lease_until" BIGINT NOT NULL, "source" TEXT NOT NULL, "peer" TEXT, "claimed_by" TEXT, "claim_generation" BIGINT NOT NULL, "claimed_at" BIGINT NOT NULL, "next_retry_at" BIGINT NOT NULL, "retry_attempts" BIGINT NOT NULL, "rounds" BIGINT NOT NULL, "max_rounds" BIGINT NOT NULL, "valid_until" BIGINT NOT NULL, "model_snapshot" TEXT NOT NULL, "effort" TEXT, "grants" TEXT NOT NULL, "memory_work" TEXT NOT NULL, "memory_cursor" BIGINT NOT NULL, "last_error" TEXT, "created_at" BIGINT NOT NULL, "updated_at" BIGINT NOT NULL, "ended_at" BIGINT NOT NULL, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("session_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("request_key", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("input_hash", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("input_event", "TEXT"),
    ColumnSpec::new("input_seq", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("final_event", "TEXT"),
    // 委派的两列都**可空**：旧行本来就没有这两个值（§8.2 的补列规则里可空那一条）。
    ColumnSpec::new("parent_run_id", "TEXT"),
    ColumnSpec::new("delegate", "TEXT"),
    ColumnSpec::new("status", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("state", "TEXT NOT NULL DEFAULT 'accepted'"),
    ColumnSpec::new("wait_kind", "TEXT"),
    ColumnSpec::new("wait_ref", "TEXT"),
    ColumnSpec::new("wake_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("lease_until", "BIGINT NOT NULL DEFAULT 0"),
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
