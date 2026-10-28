//! TUI 状态与按键处理，与终端无关，可单独测试（§13.4）。
//!
//! 这个模块**不碰终端，也不发 HTTP**：按键与服务端事件进去，状态变化加一串
//! [`Effect`] 出来，由 [`crate::tui`] 的驱动去执行。于是「Esc 在 Run 进行中 = 取消，
//! 空闲时不动草稿」这类规则是一个断言，不是一次手测。
//!
//! 执行状态由 kernel 的 [`Surface`] 折出来；树会话正文按服务端 leaf 的路径重建，旧事件
//! 仍使用 Surface 的线性消息。Surface 说不出的两件事各有一张边表：工具调用的**名字与参数**（它们在 `message.assistant` 的
//! `tool_calls` 和 `tool.planned` 的计划里，Surface 只留状态），以及每个 Run 的**开始
//! 时刻与模型**（状态行要印本轮耗时与模型，而 Surface 不记时间）。

mod events;
mod keys;
#[cfg(test)]
mod tests;
mod tokens;
mod tree;

use std::collections::BTreeMap;

use komo_kernel::fold::{Surface, SurfaceMessage};
use komo_kernel::protocol::http::{
    EventPage, InterventionAnswerResponse, InterventionKind, InterventionSummary,
    InterventionVerdict, ModelMenuEntry, NavigateSessionResponse, ResumeResponse, SessionDetail,
    SessionTreeResponse, SubmitRunResponse,
};
use komo_kernel::protocol::sse::SseFrame;
use komo_kernel::types::ids::{RequestKey, RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::model::{Effort, EffortSetting};
use komo_kernel::types::status::{RetryCause, RunState, ToolCallState, WaitReason};
use time::OffsetDateTime;

use crate::sse::ConnectionState;
use crate::tui::command::{self};
use crate::tui::model_picker::ModelPicker;
use crate::tui::paste::Input;

pub use tokens::TokenTally;

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
    /// 旧 Gateway 推来的**退役**帧（审批那两种，2026-10-11 删除）。游标照走，界面不提示。
    FrameRetired {
        id: Seq,
    },
    Connection(ConnectionState),
    /// `POST /v1/interventions/{handle}/answer` 的回执。
    Answered(Box<InterventionAnswerResponse>),
    /// `POST /v1/sessions/{id}/resume` 的结果（§8.8 的「2 个任务已接续，1 个结果不明」）。
    Resumed(Box<ResumeResponse>),
    /// `GET /v1/sessions/{id}` 的结果。
    Status(Box<SessionDetail>),
    /// 待处理清单（§7.5）：结果不明、阻塞两类一起。
    Pending(Vec<InterventionSummary>),
    /// [`Effect::RefreshPending`] 取回来的清单：只换掉那一份，不印。
    PendingRefreshed(Vec<InterventionSummary>),
    /// `GET /v1/models` 的结果。
    ModelMenu(Vec<ModelMenuEntry>),
    Tree {
        response: Box<SessionTreeResponse>,
        generation: u64,
    },
    TreeFailed {
        error: String,
        generation: u64,
    },
    TreeChanged(Box<NavigateSessionResponse>),
    TreeUpdated,
    TreeMutationFailed(String),
    Forked {
        session: SessionId,
        editor_text: Option<String>,
    },
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
        /// `text` 是 `!` 后面那条命令：不经模型直接跑（`SubmitRunRequest::shell`）。
        shell: bool,
        /// 插话（§6，`SubmitRunRequest::interject`）：有 Run 正在跑就并进它的下一次请求。
        /// 打的话一律带上——轮不轮得上由服务端判，没轮上就照常排队；`!` 命令不对话，不带。
        interject: bool,
    },
    Cancel {
        run: RunId,
        request_key: RequestKey,
    },
    /// `/new`。
    Boundary,
    /// 答一条（§7.5）：结论按种类分派。一次答一条，没有批量。
    Answer {
        handle: String,
        verdict: InterventionVerdict,
        request_key: RequestKey,
    },
    FetchPending,
    /// 悄悄再取一次清单：别处答掉的那条要从状态行上消失（[`App::poll_pending`]）。
    /// 回来的清单不印，取失败也不说——下一拍还会再问。
    RefreshPending,
    FetchStatus,
    /// `GET /v1/models`：模型清单与每个模型支持的 effort 档位。
    FetchModels,
    FetchTree {
        generation: u64,
    },
    Navigate {
        target: String,
        summarize: bool,
    },
    Fork {
        target: Option<String>,
        clone: bool,
    },
    ContextEdit {
        target: String,
        replacement: Option<serde_json::Value>,
    },
    Label {
        target: String,
        label: Option<String>,
    },
    Name(String),
    Compact {
        instructions: Option<String>,
    },
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
    /// 它出现时已经收到的最后一条事件（[`App::cursor`]）。整段重画（`Ctrl-T`）时它排回
    /// 那条事件之后，不挤到最后。
    pub at: Seq,
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
    /// 结果正文：`tool.result` 的 `body.preview`（工具写给模型的那段完整正文），没有就取
    /// `body.error`；旧事件没有 body 时退回顶层那份 ≤1 KiB 的预览。
    pub preview: Option<String>,
    pub elapsed_ms: u64,
    pub seq: Seq,
    /// 跑过阈值、转到后台的 `bash`（结果里带 `detached`）。事件上它是 completed——这次
    /// 调用确实收了尾——但命令还在跑，成败要等交回来的那条消息，所以不画成功。
    pub detached: bool,
    /// codemode 脚本里的子调用（`docs/codemode-bash.md` §3.5）：跑的时候来自 `nested_calls` 帧，
    /// 跑完了来自 `tool.result` 的 `body.result.calls`。别的工具是空的。
    pub nested: Vec<komo_kernel::protocol::nested::NestedCallView>,
}

impl ToolLine {
    /// 一行摘要：这次调用**动了什么**。参数整个印出来会把三行的界面变成三十行。
    pub fn summary(&self) -> String {
        if let Some(value) = self.summary_key().and_then(|key| self.args.get(key)) {
            return one_line(&value_text(value));
        }
        match &self.args {
            serde_json::Value::Null => String::new(),
            other => one_line(&other.to_string()),
        }
    }

    /// 摘要取的是哪个参数；一个都认不出就是 `None`，摘要是整份参数。
    pub fn summary_key(&self) -> Option<&'static str> {
        [
            "command", "code", "pattern", "path", "file", "query", "url", "task",
        ]
        .into_iter()
        .find(|key| self.args.get(key).is_some())
    }

    /// 行首那一格。
    ///
    /// **还在跑的那一格是动的**（[`crate::tui::spinner`]）：一块静止的文字只在它变的那
    /// 一刻有信息，看久了分不清是还在跑还是卡住了。工具行是一明一暗的圆点（明暗由画的
    /// 那边按 [`crate::tui::spinner::lit`] 给），与状态行那颗星分得开。跑完了反过来——
    /// 那时要的是一眼扫过去的结论，所以定住。
    ///
    /// **`?` 是 uncertain**：副作用可能已经发生（§8.6），既不是成功也不是失败，所以它
    /// 不共用失败那个符号。
    ///
    /// **`↗` 是转到后台**：调用收了尾，命令还在跑，既不是成功也不是失败。
    pub fn marker(&self, now: Option<OffsetDateTime>) -> &'static str {
        match self.state {
            ToolCallState::Planned => "·",
            ToolCallState::Started if now.is_some() => crate::tui::spinner::PULSE,
            ToolCallState::Started => crate::tui::spinner::STILL,
            ToolCallState::Completed if self.detached => "↗",
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
    /// kernel fold 的执行状态与旧格式消息；树模式正文由选中路径提供。
    pub surface: Surface,
    /// 已经收到的最大 seq——订阅从它之后续读。
    pub cursor: Seq,
    pub connection: ConnectionState,
    pub input: Input,
    /// 输入框下方的一句临时错误；不进回滚区，因为回滚区里的提示删不掉。下一次按键或粘贴时清掉。
    pub input_error: Option<String>,
    /// 发过的输入。存的是整个 [`Input`] 而不只是文字：翻回来时折起来的粘贴还是折着的。
    pub history: Vec<Input>,
    history_pos: Option<usize>,
    /// 翻历史前的草稿，连同它的折叠区。
    history_draft: Option<Input>,
    pub notices: Vec<Notice>,
    /// Enter 后立即显示；收到同一 `request_key` 的 `run.accepted` 后移除。
    pub pending_submissions: Vec<PendingSubmission>,
    /// **待处理 Intervention 的权威清单**（`GET /v1/interventions` 那一份，按句柄）。
    ///
    /// 两类一起装：结果不明、阻塞（§7.5）。不从本会话的事件折：待处理可以属于
    /// **别的会话**——定时任务那条、聊天里那条、别的 TUI 里开的那个——而操作者问的是
    /// "现在有谁在等我"。折出来的那份只覆盖本会话，于是"打开 TUI 一条也看不到"和
    /// "`/pending` 列着三条"会同时成立。
    ///
    /// 键就是线格式上的**句柄**：`verify` / `blocked` 都是 Run ID（§7.5）。状态行的条数、
    /// 提示行、`/pending` 的每一行都从这一份走，几处不可能各有各的答案。
    pub pending: BTreeMap<String, InterventionSummary>,
    /// `/pending` 问了一句——空清单也要有回答，别的时候不印。
    pub asking_pending: bool,
    pub current_run: Option<RunId>,
    /// 正在打字的那一段（[`Draft`]）。
    pub draft: Option<Draft>,
    pub runs: BTreeMap<RunId, RunMeta>,
    tools: BTreeMap<ToolCallId, ToolLine>,
    provider_calls: BTreeMap<String, ToolCallId>,
    entry_runs: BTreeMap<String, RunId>,
    /// 下一个 Run 的模型 / effort 覆盖（`/model` `/effort`）。
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub model_menu: Vec<ModelMenuEntry>,
    /// `/model` 无参时先去取一次清单；回来了才打开选择菜单。
    listing_models: bool,
    /// `/model` 的选择菜单（[`ModelPicker`]）。开着的时候按键归它，输入框禁用。
    pub model_picker: Option<ModelPicker>,
    /// Modal browsing is opened only by an explicit command, never by SSE refresh.
    pub tree_picker: Option<crate::tui::tree_picker::TreePicker>,
    pub tree: Option<SessionTreeResponse>,
    pub session_name: Option<String>,
    branch_messages: Option<Vec<SurfaceMessage>>,
    tree_generation: u64,
    tree_fetching: bool,
    tree_dirty: bool,
    tree_projection_dirty: bool,
    opening_tree: Option<crate::tui::tree_picker::TreePurpose>,
    tree_busy: bool,
    tree_mutating: bool,
    restore_tree_controls: bool,
    /// 已经提示过"有一帧读不懂"——只说一次，之后只进日志（见
    /// [`ServerEvent::FrameSkipped`]）。
    skipped_noticed: bool,
    /// 这一段对话到此为止花掉的 token（模型每答一轮报一次，见 `message.assistant`）。
    pub tokens: TokenTally,
    /// 用量算到哪一条事件为止了。**不能用 `cursor` 代替**：那一个在别处就推过了。
    counted: Seq,
    /// 工具调用全部展开还是折叠（`Ctrl-T`）。
    ///
    /// **它是一个模式，不是对某一条的展开**，管的是整段会话：交给回滚区的行终端不让改，
    /// 所以切换时整段收回、按新模式重画（[`App::repaint`]），照 pi 的 Ctrl-O。
    pub tool_detail: bool,
    /// 下一帧先擦掉屏幕与终端的回滚区，从开场横幅起整段重画。驱动取走它。
    pub repaint: bool,
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
            input_error: None,
            history: Vec::new(),
            history_pos: None,
            history_draft: None,
            notices: Vec::new(),
            pending_submissions: Vec::new(),
            pending: BTreeMap::new(),
            asking_pending: false,
            current_run: None,
            draft: None,
            runs: BTreeMap::new(),
            tools: BTreeMap::new(),
            provider_calls: BTreeMap::new(),
            entry_runs: BTreeMap::new(),
            model: None,
            effort: None,
            model_menu: Vec::new(),
            listing_models: false,
            model_picker: None,
            tree_picker: None,
            tree: None,
            session_name: None,
            branch_messages: None,
            tree_generation: 0,
            tree_fetching: false,
            tree_dirty: false,
            tree_projection_dirty: false,
            opening_tree: None,
            tree_busy: false,
            tree_mutating: false,
            restore_tree_controls: false,
            skipped_noticed: false,
            tokens: TokenTally::default(),
            counted: Seq::ZERO,
            tool_detail: false,
            repaint: false,
            now: None,
            quit: false,
            notice_seq: 0,
            key_seed: key_seed.into(),
            key_counter: 0,
        }
    }

    // ---- 读视图 ----

    pub fn messages(&self) -> &[SurfaceMessage] {
        self.branch_messages
            .as_deref()
            .unwrap_or(&self.surface.messages)
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
        let status = self.surface.runs.get(run).map(|view| view.status)?;
        // 恢复扫描把它停在等人上，这一笔只在 state.db 里、不在事件流里（§8.8）：事件折出来
        // 的还是"在跑"。清单上有它就是在等人——状态行照这个说，不让它对着一条停住的
        // Run 一直转"在跑"。
        if !status.is_terminal()
            && self
                .pending
                .values()
                .any(|item| item.run.as_ref() == Some(run))
        {
            return Some(RunState::Waiting);
        }
        Some(status)
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

    /// 当前 Run 失败或被放弃的原因。
    pub fn run_reason(&self) -> Option<&str> {
        let run = self.current_run.as_ref()?;
        self.surface
            .runs
            .get(run)
            .and_then(|view| view.reason.as_deref())
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

    /// 状态行上印的那个模型名，显式设了 effort 就跟在后面（`模型 · effort`）。
    ///
    /// Run 还在跑时印「这一轮真用的」；不在跑时印「下一个 Run 会用的」：`/model` 设的 →
    /// 上一轮用的 → 网关报的默认那一个。选完模型不再另印一行，看这一格就知道选中了什么。
    /// 最后一层要紧：没有它，第一条消息发出去之前那一格只能印"默认模型"——一句正确但没
    /// 用的话，而网关开机时就把清单报过来了，**默认是哪个它知道**。
    pub fn model_label(&self) -> String {
        let meta = self.run_meta();
        let running = self.run_state().is_some_and(|state| !state.is_terminal());
        let from_meta = || {
            meta.and_then(|meta| {
                let model = meta.model.clone()?;
                let effort = meta
                    .effort
                    .as_ref()
                    .and_then(|e| e.as_option())
                    .map(|e| e.to_string());
                Some((model, effort))
            })
        };
        let chosen = || {
            self.model
                .clone()
                .map(|model| (model, self.effort.as_ref().map(|e| e.to_string())))
        };
        let (model, effort) = if running {
            from_meta().or_else(chosen)
        } else {
            chosen().or_else(from_meta)
        }
        .unwrap_or_else(|| {
            let model = self
                .current_model_entry()
                .map(|entry| entry.id.clone())
                .unwrap_or_else(|| "默认模型".into());
            (model, self.effort.as_ref().map(|e| e.to_string()))
        });
        match effort {
            Some(effort) => format!("{model} · {effort}"),
            None => model,
        }
    }

    fn current_model_entry(&self) -> Option<&ModelMenuEntry> {
        match &self.model {
            Some(id) => self.model_menu.iter().find(|entry| &entry.id == id),
            None => self.model_menu.iter().find(|entry| entry.default),
        }
    }

    /// 界面上还没答的待处理有几条——**两类合计，全部会话的**（§7.5）。
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// 这一类现在待处理几条。
    pub fn pending_of(&self, kind: InterventionKind) -> usize {
        self.pending
            .values()
            .filter(|item| item.kind == kind)
            .count()
    }

    /// 两类的条数：结果不明、阻塞。
    pub fn pending_breakdown(&self) -> (usize, usize) {
        (
            self.pending_of(InterventionKind::Verify),
            self.pending_of(InterventionKind::Blocked),
        )
    }

    /// 按句柄找清单上那一条。
    pub fn pending_item(&self, handle: &str) -> Option<&InterventionSummary> {
        self.pending.get(handle)
    }

    /// 驱动每隔一段时间问一句：清单要不要再取一次。
    ///
    /// 答复可以来自**这个界面之外**——另一个终端的 `komo intervention answer`、聊天里的
    /// `/answer`、模型在 shell 里替人答的那一条——而答复只改 state.db，本会话的事件流
    /// 里没有它（§8.8）。所以清单上挂着东西时就定期再问，免得一条早已答过的一直挂在
    /// 状态行上。清单是空的不问：那时候没有会过期的东西。连接不在（Gateway 正在重启）
    /// 也不问：重连成功那一刻本来就会问一次。
    pub fn poll_pending(&self) -> Vec<Effect> {
        if !self.pending.is_empty() && self.connection.is_connected() {
            vec![Effect::RefreshPending]
        } else {
            Vec::new()
        }
    }

    /// 输入框里是不是一条 `!` 命令（shell 模式）。
    ///
    /// **不另记一个开关**：模式就是草稿以 `!` 开头这件事本身。退格删掉 `!` 就回到普通
    /// 模式，翻历史翻到 `!ls`、粘贴进来的 `!…` 也自然是 shell 模式——两份事实对不上的
    /// 那一刻就不会出现。
    pub fn shell_mode(&self) -> bool {
        self.input_enabled() && self.input.text().starts_with('!')
    }

    /// 模型菜单开着或正在补读历史时输入框禁用。
    pub fn input_enabled(&self) -> bool {
        self.model_picker.is_none()
            && self.tree_picker.is_none()
            && !self.tree_busy
            && self.opening_tree.is_none()
            && !self.phase.is_backfilling()
    }

    pub fn input_hint(&self) -> String {
        if let Some(picker) = &self.tree_picker {
            return format!(
                "树筛选: {} · ↑↓ 选择 · Enter 导航 · Ctrl-S 总结导航 · Ctrl-F fork · Esc 关闭",
                picker.query
            );
        }
        if self.opening_tree.is_some() {
            return "正在读取会话树…… · Esc 关闭".into();
        }
        if self.tree_busy {
            return "正在更新会话路径……".into();
        }
        let (verify, blocked) = self.pending_breakdown();
        if let Some(picker) = &self.model_picker {
            return format!("选择下一个 Run 的模型 · {}", picker.keys_hint());
        }
        if let Some(error) = &self.input_error {
            return error.clone();
        }
        if self.pending_count() > 0 {
            // 有待处理：没有弹窗，出路是 `/pending` 的清单与 `/answer`，一次答一条（§11.3）。
            // **说出路，不要只说状态**。
            return format!(
                "有 {} 条待处理（结果不明 {verify} · 阻塞 {blocked}）——/pending 看清单，\
                 /answer <句柄> <结论> 答一条",
                self.pending_count()
            );
        }
        if self.phase.is_backfilling() {
            return "正在补读历史……".to_string();
        }
        // 终端模式不说话：边框与那个 `!` 换了颜色，已经说清这一条不经模型。
        if self.shell_mode() {
            return String::new();
        }
        "Enter 发送 · Shift-Enter 换行 · Esc 停当前任务 · Ctrl-C 退出 · / 看命令 · ! 跑命令"
            .to_string()
    }

    /// 提示行当前显示的是输入错误，而不是弹窗提示。
    pub fn input_hint_is_error(&self) -> bool {
        self.input_error.is_some()
            && self.tree_picker.is_none()
            && self.opening_tree.is_none()
            && !self.tree_busy
            && self.model_picker.is_none()
    }

    /// 命令面板的候选。
    pub fn palette(&self) -> Vec<(&'static str, &'static str)> {
        if !self.input_enabled() {
            return Vec::new();
        }
        command::palette(self.input.text())
    }

    // ---- 输入 ----

    fn reject(&mut self, text: impl Into<String>) {
        self.input_error = Some(text.into());
    }

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
            at: self.cursor,
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
        detached: false,
        nested: Vec::new(),
    }
}

/// §8.8 的那句话：「2 个任务已接续，1 个结果不明」。
///
/// 待处理按 §7.5 的两类分开数——两类要的操作者做的是不同的事，合成一个数只说得出"有
/// 事没完"。
pub fn resume_summary(response: &ResumeResponse) -> Option<String> {
    let resumed = response.resumed.len();
    let mut verify = 0;
    let mut blocked = 0;
    for item in &response.pending {
        match item.kind {
            InterventionKind::Verify => verify += 1,
            InterventionKind::Blocked => blocked += 1,
        }
    }
    let mut parts = Vec::new();
    if resumed > 0 {
        parts.push(format!("{resumed} 个任务已接续"));
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
/// 状态行、提示行、`/pending` 都读清单，几个地方各说一个数就没人信了。
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

/// §7.5 的两类，如实显示。清单、提示行都要它，所以放在这里而不是各写一份。
pub fn intervention_kind_text(kind: InterventionKind) -> &'static str {
    match kind {
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
/// 接在后面印（见 [`wait_text`]），而不是在这里编出"等待干预 / 等待重试"几种状态。
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
        RunState::Absorbed => "已并入",
    }
}

/// 「停着，而且在等什么」。§8.4：**排队二十分钟不知道为什么**正是这一格要答的问题。
///
/// `now` 由驱动传进来（状态机不读时钟）；`Retry` 那一句要算"还有多久"才算说得清。
pub fn wait_text(reason: &WaitReason, now: Option<OffsetDateTime>) -> String {
    match reason {
        // 句柄（Run ID）才是操作者要用的东西，而这一格只报"在等谁"：具体那一条在 §7.5 的
        // 清单里，状态行不复述它、也不替它编一个 id。
        //
        // `Approval` 只从 2026-10-11 之前的旧行解码出来（审批已删除）：Gateway 启动时按
        // "批准"续跑它，它不等人——如实说成马上续跑，不说"等审批"。
        WaitReason::Approval { .. } => "旧的审批等待，Gateway 会直接续跑".into(),
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
