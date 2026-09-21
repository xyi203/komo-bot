//! `sessions`：连续对话、工作目录与上下文的载体（§8.1、§8.2）。

use super::{ColumnSpec, TableSpec};

/// 一个 Session 的元数据。
///
/// `applied_seq` / `applied_bytes` 是**已应用的连续前缀**：它只能推进到已经校验过的
/// 事件为止（§8.5），所以它落后于 JSONL 是正常的，超前则是损坏。
#[derive(Debug, toasty::Model)]
#[table = "sessions"]
pub struct SessionRow {
    #[key]
    pub id: String,
    pub title: String,
    /// 来源：`{platform}:{chat_id}`，或 `api` / `cron:{job}` / `home`。
    pub origin: String,
    /// 归属的助手（`AgentProfile.id`）。**空串 = 升级前建的、还没有归属**——不是"默认
    /// Agent"：谁的会话由路由认领，迁移不替它猜（[`crate::repos::session::set_agent_in`]）。
    pub agent_id: String,
    /// 用途（[`SessionKind`] 里的那三个词）。补这一列时**已有的行**按 `origin` 回填
    /// （`home` → `main`，其余 `normal`），见 [`BACKFILL_KIND`]。
    pub kind: String,
    pub workdir: Option<String>,
    pub current_run: Option<String>,
    /// 相对数据目录的 JSONL 路径。
    pub jsonl_path: String,
    pub applied_seq: i64,
    pub applied_bytes: i64,
    pub created_at: i64,
    pub updated_at: i64,
    /// 生命周期状态（§8.10）。列里就是那四个词：`active` / `closing` / `deleted` /
    /// `purged`。**认不出的值按损坏处理**，不默认成 `active`——那会把墓碑读成活会话。
    pub state: String,
    /// 状态变更时刻（unix 纳秒，哨兵 `0` = 未设置）。
    pub state_changed_at: i64,
}

pub const SPEC: TableSpec = TableSpec {
    name: "sessions",
    ddl: DDL,
    columns: COLUMNS,
};

pub const DDL: &str = r#"CREATE TABLE "sessions" ("id" TEXT NOT NULL, "title" TEXT NOT NULL, "origin" TEXT NOT NULL, "agent_id" TEXT NOT NULL, "kind" TEXT NOT NULL, "workdir" TEXT, "current_run" TEXT, "jsonl_path" TEXT NOT NULL, "applied_seq" BIGINT NOT NULL, "applied_bytes" BIGINT NOT NULL, "created_at" BIGINT NOT NULL, "updated_at" BIGINT NOT NULL, "state" TEXT NOT NULL, "state_changed_at" BIGINT NOT NULL, PRIMARY KEY ("id"))"#;

/// 补上 `kind` 之后对**已有的行**做的回填（`TableSpec` 的 `backfill`）。
///
/// 当且仅当 `origin = 'home'` 的行是主会话：它就是 `docs/bot.md` §4.2 要改成
/// `main_session(agent_id)` 的那个全局主会话，其余都是普通会话。判定写在 `WHERE kind = ''`
/// 上——这条路只在刚补完列、这些行还是空串的时候走一次，重跑什么都不改。
///
/// `agent_id` 这一列**没有回填**：空串就是"还没有归属"，迁移不替路由认主（§4.2 的
/// 「一个 Session 的 `agent_id` 创建后不随消息路由变化」）。
pub const BACKFILL_KIND: &str = r#"UPDATE "sessions" SET "kind" = CASE WHEN "origin" = 'home' THEN 'main' ELSE 'normal' END WHERE "kind" = ''"#;

pub const COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::new("id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("title", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("origin", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("agent_id", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::with_backfill("kind", "TEXT NOT NULL DEFAULT ''", BACKFILL_KIND),
    ColumnSpec::new("workdir", "TEXT"),
    ColumnSpec::new("current_run", "TEXT"),
    ColumnSpec::new("jsonl_path", "TEXT NOT NULL DEFAULT ''"),
    ColumnSpec::new("applied_seq", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("applied_bytes", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("created_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("updated_at", "BIGINT NOT NULL DEFAULT 0"),
    ColumnSpec::new("state", "TEXT NOT NULL DEFAULT 'active'"),
    ColumnSpec::new("state_changed_at", "BIGINT NOT NULL DEFAULT 0"),
];

/// `sessions.kind` 里那三个词（`docs/bot.md` §4.2：「先保持简单」——不要一开始枚举所有
/// 平台和业务场景）。
///
/// 它是**用途**，不是身份：身份是 `agent_id`。同一个 Agent 的主会话与任务会话只有 `kind`
/// 不同，两者都归它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// 一个 Agent 的**主会话**：稳定的入口（§4.2 的 `main_session(agent_id)`）。每个
    /// Agent 至多一条（活着的），见 `models::INDEXES` 里那一条部分唯一索引。
    Main,
    /// 普通会话：平台会话、`POST /v1/sessions` 建的、操作者的私聊。
    Normal,
    /// 独立任务会话：跨 Agent 委派时给目标 Agent 建的**任务**会话——不是它的主聊天
    /// （§4.2 最后一段：不要默认把子任务塞进目标 Bot 的主会话）。
    Task,
}

impl SessionKind {
    /// 列里存的那三个词。
    pub fn as_str(self) -> &'static str {
        match self {
            SessionKind::Main => "main",
            SessionKind::Normal => "normal",
            SessionKind::Task => "task",
        }
    }

    /// 列里的值 → 词汇表里的词。认不出 = `None`，调用方按**损坏**处理（与
    /// [`komo_kernel::types::status::SessionState::parse`] 一个规矩）：挑个默认值会把
    /// 一条主会话读成普通会话。
    pub fn parse(raw: &str) -> Option<SessionKind> {
        match raw {
            "main" => Some(SessionKind::Main),
            "normal" => Some(SessionKind::Normal),
            "task" => Some(SessionKind::Task),
            _ => None,
        }
    }
}
