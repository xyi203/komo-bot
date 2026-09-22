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

use std::collections::{BTreeMap, BTreeSet};

use komo_kernel::fold::{Surface, SurfaceMessage};
use komo_kernel::protocol::http::{
    ApprovalRecord, EventPage, InterventionAnswerResponse, InterventionBatchAnswerResponse,
    InterventionKind, InterventionSummary, InterventionVerdict, ModelMenuEntry, ResumeResponse,
    SessionDetail, SubmitRunResponse,
};
use komo_kernel::protocol::sse::SseFrame;
use komo_kernel::types::chat::ApprovalScope;
use komo_kernel::types::ids::{ApprovalId, RequestKey, RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::model::{Effort, EffortSetting};
use komo_kernel::types::status::{RetryCause, RunState, ToolCallState, WaitReason};
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
    /// `GET /v1/interventions/{handle}` 拿回来的那一条审批的详情。
    Approval(Box<ApprovalRecord>),
    /// `POST /v1/interventions/{handle}/answer` 的回执。
    Answered(Box<InterventionAnswerResponse>),
    /// `POST /v1/interventions/answers` 的回执（一批）。
    BatchAnswered(Box<InterventionBatchAnswerResponse>),
    /// `POST /v1/sessions/{id}/resume` 的结果（§8.8 的「2 个任务已接续，1 个等待审批」）。
    Resumed(Box<ResumeResponse>),
    /// `GET /v1/sessions/{id}` 的结果。
    Status(Box<SessionDetail>),
    /// 待处理清单（§7.5）：审批、结果不明、阻塞三类一起。
    Pending(Vec<InterventionSummary>),
    /// `GET /v1/models` 的结果。
    ModelMenu(Vec<ModelMenuEntry>),
    /// 一次操作失败。
    Failed(String),
    /// 提交 HTTP 已确认；权威的用户消息仍等 `run.accepted` 从事件流对账。
    Submitted {
        request_key: RequestKey,
        response: Box<SubmitRunResponse>,
    },
    /// 提交 HTTP 失败。带原请求键，才能把错误贴回那条本地消息。
    SubmitFailed {
        request_key: RequestKey,
        error: String,
    },
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
    /// `GET /v1/interventions/{handle}`：打开审批弹窗要的那一份详情。句柄就是短 ID。
    FetchApproval(String),
    /// 答一条（§7.5）：结论按种类分派，`scope` 只有 `approve` 用得上。
    Answer {
        handle: String,
        verdict: InterventionVerdict,
        scope: Option<ApprovalScope>,
        request_key: RequestKey,
    },
    /// 一次答一批（§11.3 的 `/approve all`）：名单由状态机列出，**只含审批**，各按本次调用。
    AnswerMany {
        handles: Vec<String>,
        approved: bool,
        request_key: RequestKey,
    },
    FetchPending,
    FetchStatus,
    /// `GET /v1/models`：模型清单与每个模型支持的 effort 档位。
    FetchModels,
    Quit,
}

/// 一条提示。
///
/// `id` 单调递增：提示一条一条交给终端回滚区，交到哪一条要有个记号，而 `notices` 这张
/// 表会从头裁（[`App::trim_notices`]），下标做不了这个记号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub id: u64,
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

/// 模型正在打字的那一段。
///
/// **不是历史**：`assistant_delta` 只在 SSE 上，永不进 JSONL（kernel 的 `SseEvent`
/// 注释说清了为什么）。所以它住在 `Surface` 外面的一个字段里——一轮结束时
/// `message.assistant` 把完整回复正式送到，草稿就地清掉，消息面上只留那一条。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub run: RunId,
    pub round: u32,
    pub text: String,
}

/// 用户已经按下 Enter、但权威 `run.accepted` 还没从事件流回来的消息。
///
/// 它只是一层可对账的界面状态，不写进 [`Surface`]，也不会进入模型回放。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSubmission {
    pub request_key: RequestKey,
    pub text: String,
    pub state: SubmissionState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionState {
    Sending,
    Submitted { run: RunId, deduplicated: bool },
    Failed { error: String },
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
    pub seq: Seq,
}

impl ToolLine {
    /// 一行摘要：这次调用**动了什么**。参数整个印出来会把三行的界面变成三十行。
    pub fn summary(&self) -> String {
        for key in [
            "command", "code", "pattern", "path", "file", "module", "query", "url",
        ] {
            if let Some(value) = self.args.get(key) {
                return one_line(&value_text(value));
            }
        }
        match &self.args {
            serde_json::Value::Null => String::new(),
            other => one_line(&other.to_string()),
        }
    }

    /// 行首那一格。
    ///
    /// **还在跑的那一格是动的**（[`crate::tui::spinner`]）：一块静止的文字只在它变的那
    /// 一刻有信息，看久了分不清是还在跑还是卡住了。跑完了反过来——那时要的是一眼扫过去
    /// 的结论，所以定住。
    ///
    /// **`?` 是 uncertain**：副作用可能已经发生（§8.6），既不是成功也不是失败，所以它
    /// 不共用失败那个符号。
    pub fn marker(&self, now: Option<OffsetDateTime>) -> &'static str {
        match self.state {
            ToolCallState::Planned => "·",
            ToolCallState::Started => crate::tui::spinner::frame(now),
            ToolCallState::Completed => "✔",
            ToolCallState::Failed => "✖",
            ToolCallState::Uncertain => "?",
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
    /// 这个界面正对着哪个会话。**`None` = 还没有**：`komo` 裸命令不在启动时建会话，
    /// 它在第一条消息送出去之前由驱动铸出来（[`crate::tui::run_tui`]）。
    pub session: Option<SessionId>,
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
    /// Enter 后立即显示；收到同一 `request_key` 的 `run.accepted` 后移除。
    pub pending_submissions: Vec<PendingSubmission>,
    pub approval: Option<ApprovalModal>,
    /// **待处理 Intervention 的权威清单**（`GET /v1/interventions` 那一份，按句柄）。
    ///
    /// 三类一起装：审批、结果不明、阻塞（§7.5）。不从本会话的事件折：待处理可以属于
    /// **别的会话**——定时任务那条、聊天里那条、别的 TUI 里开的那个——而操作者问的是
    /// "现在有谁在等我"。折出来的那份只覆盖本会话，于是"打开 TUI 一条也看不到"和
    /// "`/approve all` 说没有待处理、而 `/pending` 列着三条"会同时成立。
    ///
    /// 键就是线格式上的**句柄**：审批是短 ID，`verify` / `blocked` 是 Run ID（§7.5）。
    /// 清单里存的是摘要（句柄、种类、问题、可答结论）——审批的整份计划在弹窗那份详情里，
    /// 状态行的条数、`a` 的名单、`/pending` 的每一行都从这一份走，三处不可能各有各的答案。
    pub pending: BTreeMap<String, InterventionSummary>,
    /// **这个客户端自己答过、还在等回执的那些**（按审批层的 id）。
    ///
    /// 结论会在 SSE 上以 `approval_decided` 回来（网关每条决定推一帧），而它和"别人在
    /// 别处答的"长得一模一样，帧里也只有审批 id、没有句柄。没有这一张表，自己按下 `y`
    /// 之后收到的第一帧会被说成「这条审批在别处批准了」——一句假话，而且是在用户刚按完键
    /// 的下一秒。所以它按**弹窗上那一条的审批 id** 记：只有看得见的那一条需要这个区分，
    /// 别的那几条根本不在屏幕上。
    pub answering: BTreeSet<ApprovalId>,
    /// `/pending` 问了一句——空清单也要有回答，别的时候不印。
    pub asking_pending: bool,
    pub current_run: Option<RunId>,
    /// 正在打字的那一段（[`Draft`]）。
    pub draft: Option<Draft>,
    pub runs: BTreeMap<RunId, RunMeta>,
    tools: BTreeMap<ToolCallId, ToolLine>,
    /// 下一个 Run 的模型 / effort 覆盖（`/model` `/effort`）。
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub model_menu: Vec<ModelMenuEntry>,
    /// `/model` 无参时先去取一次清单；回来了才印。
    listing_models: bool,
    /// 已经提示过"有一帧读不懂"——只说一次，之后只进日志（见
    /// [`ServerEvent::FrameSkipped`]）。
    skipped_noticed: bool,
    /// 这一段对话到此为止花掉的 token（模型每答一轮报一次，见 `message.assistant`）。
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// 用量算到哪一条事件为止了。**不能用 `cursor` 代替**：那一个在别处就推过了。
    counted: Seq,
    /// 工具调用印不印参数与结果预览（`Ctrl-T`）。
    ///
    /// **它是一个模式，不是对某一条的展开**：一行历史交给终端回滚区之后就是终端的了，
    /// 谁也改不了它——所以这个开关管的是**之后**印出来的那些，以及此刻还在动的那几行。
    pub tool_detail: bool,
    pub now: Option<OffsetDateTime>,
    pub quit: bool,
    /// 提示的编号发到哪了。
    notice_seq: u64,
    /// 幂等键的种子。同一次操作重发要带同一个键，所以键不是随手生成的。
    key_seed: String,
    key_counter: u64,
}

impl App {
    pub fn new(session: Option<SessionId>, mode: TuiMode, key_seed: impl Into<String>) -> Self {
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
            pending_submissions: Vec::new(),
            approval: None,
            pending: BTreeMap::new(),
            answering: BTreeSet::new(),
            asking_pending: false,
            current_run: None,
            draft: None,
            runs: BTreeMap::new(),
            tools: BTreeMap::new(),
            model: None,
            effort: None,
            model_menu: Vec::new(),
            listing_models: false,
            skipped_noticed: false,
            tokens_in: 0,
            tokens_out: 0,
            counted: Seq::ZERO,
            tool_detail: false,
            now: None,
            quit: false,
            notice_seq: 0,
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

    pub fn run_state(&self) -> Option<RunState> {
        let run = self.current_run.as_ref()?;
        self.surface.runs.get(run).map(|view| view.status)
    }

    /// 这条 Run **在等什么**（§8.4 的第二个维度）。
    ///
    /// 状态只说"能不能跑"，这一格说"在等谁、等到什么时候"——少了它，状态行上的"等待中"
    /// 就是一句等于没说的话。
    pub fn run_wait(&self) -> Option<&WaitReason> {
        let run = self.current_run.as_ref()?;
        self.surface
            .runs
            .get(run)
            .and_then(|view| view.wait.as_ref())
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

    /// 可以切到的模型 id。
    pub fn model_options(&self) -> Vec<String> {
        self.model_menu
            .iter()
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// 当前这个模型支持哪几档 effort。
    ///
    /// **菜单里那一项说了算**：kernel 写明「空表就是『一档都不支持』，不是『还不知道』」
    /// ——所以找到了条目就用它的表，哪怕是空的。只有**菜单拿不到**（没取到、或当前模型
    /// 不在菜单里）才退到内建白名单，因为那时才真的是「不知道」。
    pub fn effort_options(&self) -> Vec<String> {
        match self.current_model_entry() {
            Some(entry) => entry.efforts.iter().map(|e| e.to_string()).collect(),
            None => command::EFFORT_LEVELS
                .iter()
                .map(|e| e.to_string())
                .collect(),
        }
    }

    /// 状态行上印的那个模型名。
    ///
    /// 优先级是「这一轮真用的 → `/model` 设的 → 网关报的默认那一个」。最后一层要紧：
    /// 没有它，第一条消息发出去之前那一格只能印"默认模型"——一句正确但没用的话，而网关
    /// 开机时就把清单报过来了，**默认是哪个它知道**。
    pub fn model_label(&self) -> String {
        self.run_meta()
            .and_then(|meta| meta.model.clone())
            .or_else(|| self.model.clone())
            .or_else(|| self.current_model_entry().map(|entry| entry.id.clone()))
            .unwrap_or_else(|| "默认模型".into())
    }

    fn current_model_entry(&self) -> Option<&ModelMenuEntry> {
        match &self.model {
            Some(id) => self.model_menu.iter().find(|entry| &entry.id == id),
            None => self.model_menu.iter().find(|entry| entry.default),
        }
    }

    /// 界面上还没答的待处理有几条——**三类合计，全部会话的**（§7.5）。
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// 待处理里**审批**有几条。弹窗底部那张菜单的「全部批准（N 条）」数的是它：批量只答
    /// 审批（§11.3），把结果不明和阻塞也算进去，那个数字就与按下去会发生的事不符。
    pub fn pending_approvals(&self) -> usize {
        self.pending_of(InterventionKind::Approval)
    }

    /// 这一类现在待处理几条。
    pub fn pending_of(&self, kind: InterventionKind) -> usize {
        self.pending
            .values()
            .filter(|item| item.kind == kind)
            .count()
    }

    /// 三类的条数：审批、结果不明、阻塞。
    pub fn pending_breakdown(&self) -> (usize, usize, usize) {
        (
            self.pending_of(InterventionKind::Approval),
            self.pending_of(InterventionKind::Verify),
            self.pending_of(InterventionKind::Blocked),
        )
    }

    /// 按句柄找清单上那一条。
    pub fn pending_item(&self, handle: &str) -> Option<&InterventionSummary> {
        self.pending.get(handle)
    }

    /// 有弹窗时输入框禁用（§11.3：先把眼前这件事答了）。
    pub fn input_enabled(&self) -> bool {
        self.approval.is_none() && !self.phase.is_backfilling()
    }

    pub fn input_hint(&self) -> String {
        let (approvals, verify, blocked) = self.pending_breakdown();
        if let Some(modal) = &self.approval {
            // 提示行只说**怎么开这张菜单**：菜单自己把每一行的答案与直通键写在脸上，这里
            // 再抄一遍只会在窄终端里被截掉半行（弹窗底栏的那一行同理，见
            // `ApprovalModal::keys_hint`）。
            let mut hint = format!(
                "{}——待批准 {} 条",
                if approvals > 1 {
                    "待批准的操作"
                } else {
                    "有待批准的操作"
                },
                approvals
            );
            let others = self.pending_count().saturating_sub(approvals);
            if others > 0 {
                // 弹窗旁边唯一说得出"另外那些"有出路的地方：它们没有弹窗（§7.5 的三类共用
                // 一张清单，不等于共用一种界面）。
                hint.push_str(&format!(
                    "，另有 {others} 条要答（/pending 看清单，/answer <句柄> <结论>）"
                ));
            }
            hint.push_str(&format!(" · {}", modal.keys_hint()));
            return hint;
        }
        if self.pending_count() > 0 {
            // 没弹窗却还有待处理：请求还没投到（断线、投递失败），或者那一条根本没有弹窗
            // （`verify` / `blocked`）。操作者手上有 `/pending` 的句柄，也有命令行。
            // **说出路，不要只说状态**。
            let mut hint = format!("有 {} 条待处理", self.pending_count());
            if verify > 0 || blocked > 0 {
                hint.push_str(&format!(
                    "（审批 {approvals} · 结果不明 {verify} · 阻塞 {blocked}）"
                ));
            }
            hint.push_str("——/pending 看清单");
            if approvals > 0 {
                hint.push_str("，/approve <短ID> 批一条，/approve all 全批");
            }
            if verify > 0 || blocked > 0 {
                hint.push_str(
                    "，/answer <句柄> <结论>：satisfied / not_performed / resolve / abandon",
                );
            }
            return hint;
        }
        if self.phase.is_backfilling() {
            return "正在补读历史……".to_string();
        }
        "Enter 发送 · Shift-Enter 换行 · Esc 停当前任务 · Ctrl-C 退出 · / 看命令".to_string()
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
        self.push_notice(text.into(), false);
    }

    pub fn fail(&mut self, text: impl Into<String>) {
        self.push_notice(text.into(), true);
    }

    fn push_notice(&mut self, text: String, is_error: bool) {
        self.notice_seq += 1;
        self.notices.push(Notice {
            id: self.notice_seq,
            text,
            is_error,
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
        seq,
    }
}

/// §8.8 的那句话：「2 个任务已接续，1 个等待审批」。
///
/// 待处理按 §7.5 的三类分开数——三类要的操作者做的是不同的事，合成一个数只说得出"有
/// 事没完"。
pub fn resume_summary(response: &ResumeResponse) -> Option<String> {
    let resumed = response.resumed.len();
    let mut approvals = 0;
    let mut verify = 0;
    let mut blocked = 0;
    for item in &response.pending {
        match item.kind {
            InterventionKind::Approval => approvals += 1,
            InterventionKind::Verify => verify += 1,
            InterventionKind::Blocked => blocked += 1,
        }
    }
    let mut parts = Vec::new();
    if resumed > 0 {
        parts.push(format!("{resumed} 个任务已接续"));
    }
    if approvals > 0 {
        parts.push(format!("{approvals} 个等待审批"));
    }
    if verify > 0 {
        parts.push(format!("{verify} 个结果不明"));
    }
    if blocked > 0 {
        parts.push(format!("{blocked} 个阻塞"));
    }
    // 没有未完成的任务是默认情况，不说话。
    (!parts.is_empty()).then(|| parts.join("，"))
}

/// `/status` 那一行。
///
/// 待处理数用**清单那一份**（全部会话），不是 `GET /v1/sessions/{id}` 里本会话那几条：
/// 状态行、`a` 的名单、`/pending` 都读清单，四个地方各说一个数就没人信了。三类合计之后
/// 措辞跟着改——它已经不是"待审批数"了（§7.5）。
fn status_summary(detail: &SessionDetail, pending: usize) -> String {
    let status = detail
        .summary
        .current_state
        .map(status_text)
        .unwrap_or("空闲");
    format!(
        "{status} · 未完成 {} · 待处理 {pending}",
        detail.unfinished.len()
    )
}

/// §7.5 的三类，如实显示。清单、提示行、弹窗都要它，所以放在这里而不是各写一份。
pub fn intervention_kind_text(kind: InterventionKind) -> &'static str {
    match kind {
        InterventionKind::Approval => "审批",
        InterventionKind::Verify => "结果不明",
        InterventionKind::Blocked => "阻塞",
    }
}

/// 这一条此刻允许答复什么。**空表如实印成「（无）」**——自己推一个菜单会给出一个按下去
/// 没反应的答案（§11.3）。
pub fn verdicts_text(verdicts: &[InterventionVerdict]) -> String {
    if verdicts.is_empty() {
        return "（无）".into();
    }
    verdicts
        .iter()
        .map(|verdict| verdict.as_str())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// §6 / §8.4 的状态，如实显示。
///
/// `Waiting` 只有一个词，因为**"在等什么"是另一个维度**（[`WaitReason`]）：状态行把它
/// 接在后面印（见 [`wait_text`]），而不是在这里编出"等待审批 / 等待重试"三种状态。
pub fn status_text(state: RunState) -> &'static str {
    match state {
        RunState::Accepted => "已受理",
        RunState::Queued => "排队中",
        RunState::Running => "运行中",
        RunState::Waiting => "等待中",
        RunState::Completed => "已完成",
        RunState::Failed => "已失败",
        RunState::Cancelled => "已取消",
        RunState::Abandoned => "已放弃",
    }
}

/// 「停着，而且在等什么」。§8.4：**排队二十分钟不知道为什么**正是这一格要答的问题。
///
/// `now` 由驱动传进来（状态机不读时钟）；`Retry` 那一句要算"还有多久"才算说得清。
pub fn wait_text(reason: &WaitReason, now: Option<OffsetDateTime>) -> String {
    match reason {
        // 句柄才是操作者要用的东西（审批是短 ID，另两类是 Run ID），而这一格只报"在等谁"：
        // 具体那一条在 §7.5 的清单里，状态行不复述它、也不替它编一个 id。
        WaitReason::Approval { .. } => "等审批答复".into(),
        WaitReason::Intervention { .. } => "等一条干预的答复".into(),
        WaitReason::Dependency { run } => format!("在等 Run {run}"),
        WaitReason::Retry {
            attempts,
            not_before,
            cause,
        } => {
            let remaining = match now {
                Some(now) if *not_before > now => {
                    format!("{} 后", human_span(*not_before - now))
                }
                // 到点了却还停着：下一拍就会重试，别报一个负数的"还要等"。
                _ => "马上".into(),
            };
            format!(
                "{remaining}重试（{}，第 {attempts} 次）",
                retry_cause_text(*cause)
            )
        }
    }
}

/// 一次有界退避是**哪一种**失败（§8.5）：分类不是为了好看，是让人一眼知道该不该干预。
fn retry_cause_text(cause: RetryCause) -> &'static str {
    match cause {
        RetryCause::RateLimited => "限流",
        RetryCause::Transport => "连不上",
        RetryCause::Server => "服务端错误",
        RetryCause::Contended => "本地写争用",
    }
}

/// 一段时长的人话。
fn human_span(duration: time::Duration) -> String {
    let seconds = duration.whole_seconds().max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}
