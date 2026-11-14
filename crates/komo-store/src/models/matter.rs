//! `matters`：目标与领域（`docs/goals.md` §3）。耐久表。

use super::{ColumnSpec, TableSpec};

/// 一个目标或领域。两种一张表，`kind` 区分；种类专属的列在另一种上恒为 NULL。
#[derive(Debug, toasty::Model)]
#[table = "matters"]
pub struct MatterRow {
    #[key]
    pub id: String,
    pub revision: i64,
    /// `MatterKind`：goal / area。
    pub kind: String,
    pub title: String,
    /// `MatterState`：active / waiting / parked / done / dropped / retired。
    pub state: String,
    /// 只给 goal：下一步。
    pub next: Option<String>,
    /// 只给 goal：在等谁、等什么。
    pub waiting_for: Option<String>,
    /// 只给 goal：截止日期，`YYYY-MM-DD`。
    pub due: Option<String>,
    /// 只给 area：要维持的标准。
    pub standard: Option<String>,
    /// `Vec<Evidence>` 的 JSON，追加不改。
    pub evidence: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// `0` = 未结案。
    pub closed_at: i64,
    /// `Vec<MatterTrigger>` 的 JSON（P1e，附加列）。
    pub triggers: String,
    /// 说明（`docs/redesign.md` §2.2，附加列）。
    pub description: Option<String>,
    /// 定时配置（定时并进目标，附加列）：`NULL` = 没有定时；否则是定时定义的 JSON（版本、触发器、
    /// 提示词 / 命令、模型覆盖、通知规则……，见 `repos::cron::ScheduleDef`）。一个目标至多一个。
    pub schedule: Option<String>,
    /// 定时的启停：`JobStatus` 的 active / paused / done。只在有定时时有意义。
    pub schedule_status: String,
    /// 调度器唯一找得到的槽位；`0` = 没有下一次。带索引（`matters_next_run`）。
    pub next_run_at: i64,
    /// 触发 / 配置层面的问题（算不出下一槽等）；执行失败记在 `cron_firings` 上。
    pub last_error: Option<String>,
    /// 这个目标是回填自哪个旧定时任务（`cron_jobs.id`）；回填据此判断「这个旧任务已经处理过」，
    /// 即使之后定时被清掉也不会再造出一个目标。
    pub migrated_from: Option<String>,
    /// 绑定的支线对话（会话 id，`docs/redesign.md` P22，附加列）；`NULL` = 没绑定。一个目标至多一段，
    /// 一段支线至多被一个目标绑（只能从目标这边新开一段来绑，所以天然一对一）。改它不升 `revision`。
    pub chat_session: Option<String>,
}

pub const SPEC: TableSpec = TableSpec {
    name: "matters",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "matters" ("id" TEXT NOT NULL, "revision" BIGINT NOT NULL, "kind" TEXT NOT NULL, "title" TEXT NOT NULL, "state" TEXT NOT NULL, "next" TEXT, "waiting_for" TEXT, "due" TEXT, "standard" TEXT, "evidence" TEXT NOT NULL, "created_at" BIGINT NOT NULL, "updated_at" BIGINT NOT NULL, "closed_at" BIGINT NOT NULL, "triggers" TEXT NOT NULL, "description" TEXT, "schedule" TEXT, "schedule_status" TEXT NOT NULL, "next_run_at" BIGINT NOT NULL, "last_error" TEXT, "migrated_from" TEXT, "chat_session" TEXT, PRIMARY KEY ("id"))"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("revision", "BIGINT NOT NULL DEFAULT 1"),
    ColumnSpec::new("kind", "TEXT NOT NULL DEFAULT 'goal'"),
    ColumnSpec::new("title", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("state", "TEXT NOT NULL DEFAULT 'active'"),
    ColumnSpec::new("next", "TEXT"),
    ColumnSpec::new("waiting_for", "TEXT"),
    ColumnSpec::new("due", "TEXT"),
    ColumnSpec::new("standard", "TEXT"),
    ColumnSpec::new("evidence", "TEXT NOT NULL DEFAULT '[]'"),
    ColumnSpec::new("created_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("updated_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("closed_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("triggers", "TEXT NOT NULL DEFAULT '[]'"),
    ColumnSpec::new("description", "TEXT"),
    ColumnSpec::new("schedule", "TEXT"),
    ColumnSpec::new("schedule_status", "TEXT NOT NULL DEFAULT 'active'"),
    ColumnSpec::new("next_run_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("last_error", "TEXT"),
    ColumnSpec::new("migrated_from", "TEXT"),
    ColumnSpec::new("chat_session", "TEXT"),
];
