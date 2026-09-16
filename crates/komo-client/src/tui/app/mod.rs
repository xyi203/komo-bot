//! TUI 状态与按键处理，与终端无关，可单独测试（§13.4）。
//!
//! 这个模块**不碰终端，也不发 HTTP**：按键与服务端事件进去，状态变化加一串
//! [`Effect`] 出来，由 [`crate::tui`] 的驱动去执行。于是「Esc 在 Run 进行中 = 取消，
//! 空闲时不动草稿」这类规则是一个断言，不是一次手测。
//!
//! 消息面由 kernel 的 [`Surface`] 折出来——**这里不写第二套折叠**。Surface 说不出的
//! 两件事各有一张边表：工具调用的**名字与参数**（它们在 `message.assistant` 的
//! `tool_calls` 和 `tool.planned` 的计划里，Surface 只留状态），以及每个 Run 的**开始
//! 时刻与模型**（状态行要印本轮耗时与模型，而 Surface 不记时间）。

mod events;
mod keys;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use komo_kernel::fold::{Surface, SurfaceMessage};
use komo_kernel::protocol::http::{
    ApprovalRecord, EventPage, PendingItem, ResumeResponse, SessionDetail,
};
use komo_kernel::protocol::sse::SseFrame;
use komo_kernel::types::chat::ApprovalScope;
use komo_kernel::types::ids::{ApprovalId, RequestKey, RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::model::{Effort, EffortSetting};
use komo_kernel::types::status::{RunStatus, ToolCallState};
use time::OffsetDateTime;

use crate::sse::ConnectionState;
use crate::tui::approval::ApprovalModal;
use crate::tui::command::{self};
use crate::tui::paste::Input;

/// TUI 打开的方式（§3 命令表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiMode {
    /// `komo`：新会话。
    New,
    /// `komo resume SESSION_ID`：连接原会话，**先补读全部历史再进入交互**（§8.8）。
    Resume,
    /// 操作者的那一个常驻会话（§11.2 的 home session）。
    Home,
}

impl TuiMode {
    /// 身份行上的一句话。
    pub fn label(self) -> &'static str {
        match self {
            TuiMode::New => "新会话",
            TuiMode::Resume => "续接会话",
            TuiMode::Home => "home 会话",
        }
    }

    /// 打开时要不要先补读历史。
    pub fn needs_backfill(self) -> bool {
        matches!(self, TuiMode::Resume | TuiMode::Home)
    }
}

/// 界面处在哪一段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// 正在按游标补读历史。**这一段不接受提交**——把一条消息插到还没读完的历史前面，
    /// 就是让模型看见一段它没有的上下文。
    Backfilling {
        events: usize,
    },
    Interactive,
}

impl Phase {
    pub fn is_backfilling(&self) -> bool {
        matches!(self, Phase::Backfilling { .. })
    }
}

/// 驱动喂给状态机的东西。
#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    /// 补读到的一页历史（`GET /v1/sessions/{id}/events`）。
    HistoryPage(Box<EventPage>),
    /// 历史读完了，可以进入交互。
    HistoryDone,
    /// 订阅收到的一帧。
    Frame(Box<SseFrame>),
    /// 这个版本读不懂的一帧。游标已经前进。
    FrameSkipped {
        id: Seq,
        reason: String,
    },
    Connection(ConnectionState),
    /// `GET /v1/approvals/{id}` 的结果。
    Approval(Box<ApprovalRecord>),
    /// `POST /v1/approvals/{id}/decision` 的回执。
    ApprovalSettled {
        approval: ApprovalId,
        approved: bool,
        already_decided: bool,
    },
    /// `POST /v1/sessions/{id}/resume` 的结果（§8.8 的「2 个任务已接续，1 个等待审批」）。
    Resumed(Box<ResumeResponse>),
    /// `GET /v1/sessions/{id}` 的结果。
    Status(Box<SessionDetail>),
    /// 待处理审批清单。
    Pending(Vec<ApprovalRecord>),
    /// 可选的模型清单。
    ModelMenu(Vec<String>),
    /// 一次操作失败。
    Failed(String),
    Notice(String),
    /// 时钟推进——耗时是**驱动**读的钟，状态机不读时钟。
    Tick(OffsetDateTime),
}

/// 状态机要驱动去做的事。
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Submit {
        request_key: RequestKey,
        text: String,
        model: Option<String>,
        effort: Option<Effort>,
    },
    Cancel {
        run: RunId,
        request_key: RequestKey,
    },
    /// `/new`。
    Boundary,
    FetchApproval(ApprovalId),
    Decide {
        approval: ApprovalId,
        approved: bool,
        scope: ApprovalScope,
        request_key: RequestKey,
    },
    FetchPending,
    FetchStatus,
    Quit,
}

/// 一条提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub is_error: bool,
}

/// 一个 Run 在状态行上要印的东西。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunMeta {
    pub started_at: Option<OffsetDateTime>,
    pub ended_at: Option<OffsetDateTime>,
    pub model: Option<String>,
    pub effort: Option<EffortSetting>,
}

/// 一次工具调用在界面上的一行。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolLine {
    pub call: ToolCallId,
    pub run: Option<RunId>,
    pub tool: String,
    pub state: ToolCallState,
    pub attempts: u32,
    pub args: serde_json::Value,
    pub cwd: Option<String>,
    pub preview: Option<String>,
    pub elapsed_ms: u64,
    pub expanded: bool,
    pub seq: Seq,
}

impl ToolLine {
    /// 一行摘要：这次调用**动了什么**。参数整个印出来会把三行的界面变成三十行。
    pub fn summary(&self) -> String {
        for key in ["command", "code", "path", "file", "module", "query", "url"] {
            if let Some(value) = self.args.get(key) {
                return one_line(&value_text(value));
            }
        }
        match &self.args {
            serde_json::Value::Null => String::new(),
            other => one_line(&other.to_string()),
        }
    }

    /// 状态标记。**`??` 是 uncertain**：副作用可能已经发生（§8.6），既不是成功也不是失败。
    pub fn marker(&self) -> &'static str {
        match self.state {
            ToolCallState::Planned => "··",
            ToolCallState::Started => "▶ ",
            ToolCallState::Completed => "ok",
            ToolCallState::Failed => "!!",
            ToolCallState::Uncertain => "??",
        }
    }
}

fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn one_line(text: &str) -> String {
    let flat: String = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ⏎ ");
    flat
}

/// TUI 的全部状态。
#[derive(Debug, Clone)]
pub struct App {
    pub session: SessionId,
    pub mode: TuiMode,
    pub phase: Phase,
    /// 由 kernel 的 fold 折出来的消息面。
    pub surface: Surface,
    /// 已经收到的最大 seq——订阅从它之后续读。
    pub cursor: Seq,
    pub connection: ConnectionState,
    pub input: Input,
    pub history: Vec<String>,
    history_pos: Option<usize>,
    /// 翻历史前的草稿。
    history_draft: Option<String>,
    pub notices: Vec<Notice>,
    pub approval: Option<ApprovalModal>,
    /// 还没取到详情的待处理审批（`approval.pending` 只给 id）。
    pub pending_short_ids: BTreeMap<ApprovalId, komo_kernel::types::ids::ShortId>,
    pub current_run: Option<RunId>,
    pub runs: BTreeMap<RunId, RunMeta>,
    tools: BTreeMap<ToolCallId, ToolLine>,
    /// 下一个 Run 的模型 / effort 覆盖（`/model` `/effort`）。
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub model_menu: Vec<String>,
    /// 消息面滚动：从底部往上数多少行。0 = 贴底。
    pub scroll: u16,
    pub now: Option<OffsetDateTime>,
    pub quit: bool,
    /// 幂等键的种子。同一次操作重发要带同一个键，所以键不是随手生成的。
    key_seed: String,
    key_counter: u64,
}

impl App {
    pub fn new(session: SessionId, mode: TuiMode, key_seed: impl Into<String>) -> Self {
        App {
            session,
            mode,
            phase: if mode.needs_backfill() {
                Phase::Backfilling { events: 0 }
            } else {
                Phase::Interactive
            },
            surface: Surface::default(),
            cursor: Seq::ZERO,
            connection: ConnectionState::Connecting,
            input: Input::new(),
            history: Vec::new(),
            history_pos: None,
            history_draft: None,
            notices: Vec::new(),
            approval: None,
            pending_short_ids: BTreeMap::new(),
            current_run: None,
            runs: BTreeMap::new(),
            tools: BTreeMap::new(),
            model: None,
            effort: None,
            model_menu: Vec::new(),
            scroll: 0,
            now: None,
            quit: false,
            key_seed: key_seed.into(),
            key_counter: 0,
        }
    }

    // ---- 读视图 ----

    pub fn messages(&self) -> &[SurfaceMessage] {
        &self.surface.messages
    }

    /// 工具调用，按它们出现的顺序。
    pub fn tool_lines(&self) -> Vec<&ToolLine> {
        let mut lines: Vec<&ToolLine> = self.tools.values().collect();
        lines.sort_by_key(|line| line.seq);
        lines
    }

    pub fn tool(&self, call: &ToolCallId) -> Option<&ToolLine> {
        self.tools.get(call)
    }

    pub fn run_status(&self) -> Option<RunStatus> {
        let run = self.current_run.as_ref()?;
        self.surface.runs.get(run).map(|view| view.status)
    }

    pub fn run_meta(&self) -> Option<&RunMeta> {
        self.runs.get(self.current_run.as_ref()?)
    }

    /// 本轮耗时。Run 结束后定格在它结束的那一刻。
    pub fn elapsed(&self) -> Option<time::Duration> {
        let meta = self.run_meta()?;
        let started = meta.started_at?;
        let end = meta.ended_at.or(self.now)?;
        Some(end - started)
    }

    /// 界面上还没决定的审批有几条。
    pub fn pending_count(&self) -> usize {
        self.surface.pending_approvals.len()
    }

    /// 有审批弹窗时输入框禁用（§11.3：先把眼前这件事答了）。
    pub fn input_enabled(&self) -> bool {
        self.approval.is_none() && !self.phase.is_backfilling()
    }

    pub fn input_hint(&self) -> &'static str {
        if self.approval.is_some() {
            "有待批准的操作——先 y / r 批准或 n / Esc 拒绝"
        } else if self.phase.is_backfilling() {
            "正在补读历史……"
        } else {
            "Enter 发送 · Shift/Alt-Enter 或 Ctrl-J 换行 · Ctrl-T 展开工具 · / 看命令"
        }
    }

    /// 命令面板的候选。
    pub fn palette(&self) -> Vec<(&'static str, &'static str)> {
        if !self.input_enabled() {
            return Vec::new();
        }
        command::palette(self.input.text())
    }

    // ---- 输入 ----

    pub fn note(&mut self, text: impl Into<String>) {
        self.notices.push(Notice {
            text: text.into(),
            is_error: false,
        });
        self.trim_notices();
    }

    pub fn fail(&mut self, text: impl Into<String>) {
        self.notices.push(Notice {
            text: text.into(),
            is_error: true,
        });
        self.trim_notices();
    }

    fn trim_notices(&mut self) {
        const KEEP: usize = 50;
        if self.notices.len() > KEEP {
            self.notices.drain(..self.notices.len() - KEEP);
        }
    }

    fn next_key(&mut self, what: &str) -> RequestKey {
        self.key_counter += 1;
        RequestKey::new(format!("tui:{}:{what}:{}", self.key_seed, self.key_counter))
    }
}

fn blank_tool(call: ToolCallId, run: Option<RunId>, seq: Seq) -> ToolLine {
    ToolLine {
        call,
        run,
        tool: "?".into(),
        state: ToolCallState::Planned,
        attempts: 0,
        args: serde_json::Value::Null,
        cwd: None,
        preview: None,
        elapsed_ms: 0,
        expanded: false,
        seq,
    }
}

/// §8.8 的那句话：「2 个任务已接续，1 个等待审批」。
pub fn resume_summary(response: &ResumeResponse) -> String {
    let resumed = response.resumed.len();
    let mut approvals = 0;
    let mut uncertain = 0;
    let mut attention = 0;
    for item in &response.pending {
        match item {
            PendingItem::Approval(_) => approvals += 1,
            PendingItem::Uncertain { .. } => uncertain += 1,
            PendingItem::NeedsAttention { .. } => attention += 1,
        }
    }
    let mut parts = Vec::new();
    if resumed > 0 {
        parts.push(format!("{resumed} 个任务已接续"));
    }
    if approvals > 0 {
        parts.push(format!("{approvals} 个等待审批"));
    }
    if uncertain > 0 {
        parts.push(format!("{uncertain} 个结果不明"));
    }
    if attention > 0 {
        parts.push(format!("{attention} 个需要处理"));
    }
    if parts.is_empty() {
        "没有未完成的任务".to_string()
    } else {
        parts.join("，")
    }
}

fn status_summary(detail: &SessionDetail) -> String {
    let status = detail
        .summary
        .current_status
        .map(status_text)
        .unwrap_or("空闲");
    format!(
        "{status} · 未完成 {} · 待审批 {}",
        detail.unfinished.len(),
        detail.pending_approvals.len()
    )
}

/// §6 / §8.4 的十个状态，如实显示。
pub fn status_text(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Ingesting => "接收中",
        RunStatus::Queued => "排队中",
        RunStatus::Running => "运行中",
        RunStatus::WaitingApproval => "等待审批",
        RunStatus::WaitingRetry => "等待重试",
        RunStatus::Interrupted => "被中断",
        RunStatus::NeedsAttention => "需要处理",
        RunStatus::Completed => "已完成",
        RunStatus::Failed => "已失败",
        RunStatus::Cancelled => "已取消",
    }
}
