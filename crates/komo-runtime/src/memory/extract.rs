//! LearningPass：一个会话空闲后一批 Run 的窗口 → 结构化观察（§9.3）。
//!
//! 「记忆模型提取结构化候选，**不提供执行工具** → **Rust 校验**来源角色、引用位置、
//! 作用域与内容」——这两步在这个文件里是两段代码，中间没有捷径：
//!
//! - [`LearningPass::extract`] 只负责发一次请求、把 JSON 解出来。它交出的是
//!   [`RawObservation`]，一个**模型说了什么**的记录，还不是一条记忆。
//! - [`Transcript::validate`] 把 raw 变成 [`Observation`]。这里落三条硬规则：
//!   证据必须指向**这一批 Run 里真实存在的**用户消息或工具结果；来源角色由证据决定而不
//!   是由模型自称决定；`user_confirmed` 根本没有被反序列化进来——模型返回的那个字段
//!   没有写入权限（§9.2），做法是这组类型里没有它的位置。

use std::collections::BTreeMap;
use std::sync::Arc;

use komo_kernel::events::{Event, EventPayload};
use komo_kernel::traits::{Clock, LlmClient};
use komo_kernel::types::ids::{EventId, MemoryId, RunId, Seq, SessionId};
use komo_kernel::types::memory::{
    Confirmation, Evidence, EvidenceRef, ExtractionMetadata, MemoryApplication, MemoryItem,
    MemoryKind, MemoryScope, MemoryState, Provenance,
};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::status::RunState;
use komo_kernel::types::turn::{MemoryUse, RoundInput, ToolCallRequest, TurnRequest};
use serde::Deserialize;
use time::OffsetDateTime;

use super::MemoryError;
use super::consolidate::{RawVerdict, Verdict, map_verdicts};

/// 提示词版本。**改了提示词就改它**——它进 [`ExtractionMetadata`]，是"这条记忆是怎么来
/// 的"里唯一能回答"按哪一版规则提的"的那一格（§9.2）。
pub const PROMPT_VERSION: &str = "memory-extract/v8";

/// 一条记忆正文的上限。再长就不是"简短、尽量单一的陈述"了（§9.2）。
pub(super) const MAX_CONTENT_CHARS: usize = 300;
/// 会话窗口里每次工具调用的参数最多留几个字符——够看出读的是哪个文件、跑的是哪条命令。
const MAX_CALL_ARGUMENT_CHARS: usize = 200;
/// 交给记忆模型的会话窗口上限（字符）。
const MAX_TRANSCRIPT_CHARS: usize = 24_000;

/// 模型自称这条是谁说的。**只是"自称"**——[`Transcript::validate`] 拿证据核对它。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaidBy {
    UserStatement,
    ToolObservation,
    ModelInference,
}

impl SaidBy {
    fn as_provenance(self) -> Provenance {
        match self {
            SaidBy::UserStatement => Provenance::UserStatement,
            SaidBy::ToolObservation => Provenance::ToolObservation,
            SaidBy::ModelInference => Provenance::ModelInference,
        }
    }
}

/// 模型交回来的一条。
///
/// **注意这里没有 `user_confirmed`、没有 `state`、没有 `revision`**：确认等级与生命周期
/// 不是模型能写的东西（§9.2），而 serde 会把它们连同任何别的字段一起丢掉。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RawObservation {
    pub content: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub scope: String,
    pub said_by: SaidBy,
    /// 证据引用：这一批 Run 的事件 ID。
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub observed_at: Option<String>,
    #[serde(default)]
    pub valid_until: Option<String>,
    /// 与编号候选的关系（§9.10）。先按原样收下，[`Transcript::validate`] 里逐条解、
    /// 再用编号表映射回 id——一条坏的不拖累整批。
    #[serde(default)]
    pub relations: Vec<serde_json::Value>,
    /// 模型自称的使用方式：`convention` / `reference`（§9.2）。也只是"自称"，
    /// [`Transcript::validate`] 对 convention 逐条核对，不合格降为 reference。
    #[serde(default)]
    pub application: Option<String>,
    /// convention 的条件，写成一句能独立理解的话；reference 可省。
    #[serde(default)]
    pub applies_when: Option<String>,
    /// convention 的依据：所引用户事件里表达长期意图与条件的原话片段。**只用于校验，
    /// 不落库**（§9.3）。
    #[serde(default)]
    pub basis: Option<String>,
}

fn default_kind() -> String {
    "fact".into()
}

/// 模型一次交回的全部：观察，加上"这一批注入的记忆里哪些被用上了"（§9.10）。
#[derive(Debug, Default, Deserialize)]
pub struct RawExtraction {
    #[serde(default)]
    pub observations: Vec<RawObservation>,
    /// 采纳判断，先按原样收下，[`Transcript::validate_used`] 逐条解——一条坏的不拖累整批。
    #[serde(default)]
    pub used: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RawUse {
    memory: serde_json::Value,
    #[serde(default)]
    evidence: Vec<String>,
}

/// 一条**校验过**的观察：来源角色对得上证据，引用指向真实存在的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub content: String,
    pub kind: MemoryKind,
    pub scope: MemoryScope,
    pub provenance: Provenance,
    /// 入库时的初始状态：用户原话 / 工具观察 `active`，模型推断 `candidate`（§9.2）。
    pub state: MemoryState,
    pub evidence: Vec<Evidence>,
    pub observed_at: OffsetDateTime,
    pub valid_until: Option<OffsetDateTime>,
    pub extraction: ExtractionMetadata,
    /// 与已有记忆的关系，编号已映射成 id（§9.9）。
    pub relations: Vec<Verdict>,
    /// 校验过的使用方式。提取一律给出 `Some`：没通过 convention 校验的是 reference。
    pub application: Option<MemoryApplication>,
    pub applies_when: Option<String>,
}

impl Observation {
    /// 入库时的确认等级。**永远是 `Unconfirmed`**：只有操作者的 `confirm` 抬得起来
    /// （§9.2），而这个函数是提取这条路上唯一填它的地方。
    pub fn confirmation(&self) -> Confirmation {
        Confirmation::Unconfirmed
    }
}

/// 一批里的一个 Run：它的终态与处理游标（§9.3）。
#[derive(Debug, Clone)]
pub struct RunWindow {
    pub run: RunId,
    pub status: RunState,
    /// `cursor` 之前的事件不再算新证据。
    pub cursor: Seq,
}

/// 一批 Run（同一会话空闲后的全部待处理 Run，§9.10）的会话窗口，连同"哪条事件是谁
/// 说的"的索引。
#[derive(Debug, Clone)]
pub struct Transcript {
    pub session: SessionId,
    /// 窗口里最后一个 Run；请求要挂一个 Run id，用它。
    pub run: RunId,
    /// 渲染给记忆模型看的正文。
    pub rendered: String,
    /// 每个 Run 处理完推进到哪条 seq。
    cursors: BTreeMap<RunId, Seq>,
    /// 窗口里用户输入行的字符数：判断一批要不要按 Run 边界切开用。
    user_chars: usize,
    statuses: BTreeMap<RunId, RunState>,
    /// 新发生的用户陈述，一行一条：提取前召回已有记忆的查询文本（§9.10）。
    statements: String,
    /// 事件 ID → （所属 Run，seq，角色）。**校验引用位置**用的就是它。
    roles: BTreeMap<String, (RunId, Seq, Role)>,
    /// 用户事件 ID → 空白归一后的完整原文：convention 的 `basis` 要在它里面逐字出现。
    user_texts: BTreeMap<String, String>,
    /// 这一批 Run 的系统提示里注入过的记忆（去重、按首次出现排序）。
    injected: Vec<MemoryUse>,
    /// `memory = "private"` 的 Profile：这一批只许写进这一个作用域（`agent:<id>`）。模型
    /// 返回什么作用域都不算，[`Transcript::validate`] 一律改写成它。`None` = 模型自己选。
    confined: Option<MemoryScope>,
}

/// 一条事件在提取里算什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// 用户陈述。
    User,
    /// 系统交回会话的内容：任务结果（`task-result:`）、监听事件与结束（`monitor:` /
    /// `monitor-end:`）、转后台命令的结束（`bash-done:`）。**不是用户说的话**，也不能当证据。
    TaskResult,
    /// 可验证的工具结果。
    Tool,
    /// 助手自己说的话。**不算新的独立证据**（§9.3）。
    Assistant,
}

struct Line {
    event_id: String,
    role: Role,
    text: String,
}

impl Line {
    fn chars(&self) -> usize {
        self.text.chars().count() + 1
    }
}

/// 系统交回内容时 `run.accepted` 的请求键前缀（`service/tasks.rs`、`service/monitors.rs`）。
pub(super) const HANDBACK_KEY_PREFIXES: [&str; 4] =
    ["task-result:", "monitor:", "monitor-end:", "bash-done:"];

impl Transcript {
    /// 把一批 Run 的事件按 seq 顺序渲染成一个窗口。每个 Run 的 `cursor` 之前的不再算新
    /// 证据（§9.3 的来源游标）。
    ///
    /// 超过 [`MAX_TRANSCRIPT_CHARS`] 时**用户输入行一行不丢**：先从最旧的工具结果删起，
    /// 再删最旧的助手 / 任务交回行（§9.10）。
    pub fn build(session: &SessionId, runs: &[RunWindow], events: &[Event]) -> Transcript {
        let mut roles = BTreeMap::new();
        let mut user_texts = BTreeMap::new();
        let mut lines: Vec<Line> = Vec::new();
        let mut cursors: BTreeMap<RunId, Seq> =
            runs.iter().map(|w| (w.run.clone(), w.cursor)).collect();
        let windows: BTreeMap<&RunId, &RunWindow> = runs.iter().map(|w| (&w.run, w)).collect();
        // call_id → 「工具名 参数」。工具结果事件自己不带工具名；记忆模型要靠它分清
        // "读文档读到的"和"排查跑出来的"，前者随时能重读，不值得记。
        let mut calls: BTreeMap<String, String> = BTreeMap::new();
        let mut statements: Vec<String> = Vec::new();

        for event in events {
            // 只看这一批 Run 的证据。
            let Some(run) = event.run.as_ref() else {
                continue;
            };
            let Some(window) = windows.get(run) else {
                continue;
            };
            let highest = cursors.entry(run.clone()).or_insert(window.cursor);
            *highest = (*highest).max(event.seq);
            let fresh = event.seq > window.cursor;
            let (role, body) = match &event.payload {
                EventPayload::RunAccepted(body) => (
                    if HANDBACK_KEY_PREFIXES
                        .iter()
                        .any(|prefix| body.request_key.as_str().starts_with(prefix))
                    {
                        Role::TaskResult
                    } else {
                        Role::User
                    },
                    body.text.clone().unwrap_or_else(|| "（正文外置）".into()),
                ),
                EventPayload::MessageUser(body) => (
                    Role::User,
                    body.text.clone().unwrap_or_else(|| "（正文外置）".into()),
                ),
                EventPayload::MessageAssistant(body) => {
                    let mut text = body.text.clone().unwrap_or_default();
                    for call in &body.tool_calls {
                        let summary = call_summary(call);
                        if !text.is_empty() {
                            text.push(' ');
                        }
                        text.push_str(&format!("（调用 {summary}）"));
                        calls.insert(call.call_id.to_string(), summary);
                    }
                    if text.is_empty() {
                        text = "（空回复）".into();
                    }
                    (Role::Assistant, text)
                }
                EventPayload::ToolResult(body) => (
                    Role::Tool,
                    format!(
                        "{} · {:?} · {}",
                        calls
                            .get(&body.call_id.to_string())
                            .map(String::as_str)
                            .unwrap_or("（调用未知）"),
                        body.status,
                        body.preview.clone().unwrap_or_else(|| "（无预览）".into())
                    ),
                ),
                _ => continue,
            };
            roles.insert(event.event_id.to_string(), (run.clone(), event.seq, role));
            if role == Role::User {
                user_texts.insert(event.event_id.to_string(), squash_whitespace(&body));
            }
            // 旧证据仍然渲染给模型当上下文，但标出来——它不是新发生的事。
            let seen = if fresh { "" } else { " · 已处理过" };
            if fresh && role == Role::User {
                statements.push(one_line(&body));
            }
            lines.push(Line {
                event_id: event.event_id.to_string(),
                role,
                text: format!(
                    "[{} seq={} {}{seen}] {}",
                    event.event_id,
                    event.seq,
                    label(role),
                    one_line(&body)
                ),
            });
        }

        let user_chars = lines
            .iter()
            .filter(|line| line.role == Role::User)
            .map(Line::chars)
            .sum();

        let mut total: usize = lines.iter().map(Line::chars).sum();
        let mut dropped = 0usize;
        if total > MAX_TRANSCRIPT_CHARS {
            let mut keep = vec![true; lines.len()];
            for class in [
                [Role::Tool, Role::Tool],
                [Role::Assistant, Role::TaskResult],
            ] {
                for (index, line) in lines.iter().enumerate() {
                    if total <= MAX_TRANSCRIPT_CHARS {
                        break;
                    }
                    if class.contains(&line.role) {
                        keep[index] = false;
                        total -= line.chars();
                        dropped += 1;
                    }
                }
            }
            let mut index = 0;
            lines.retain(|line| {
                let kept = keep[index];
                index += 1;
                if !kept {
                    roles.remove(&line.event_id);
                }
                kept
            });
        }

        // 每个 Run 注入了什么：它结束前最近的一条 `memory.injected`（沿用上一块的 Run 自己
        // 没有事件）。
        let mut injected: Vec<MemoryUse> = Vec::new();
        for window in runs {
            let upto = cursors.get(&window.run).copied().unwrap_or(window.cursor);
            if let Some((_, body)) = komo_kernel::events::injection_at(events, upto) {
                for use_ in &body.uses {
                    if !injected.iter().any(|seen| seen.memory == use_.memory) {
                        injected.push(use_.clone());
                    }
                }
            }
        }

        let mut rendered = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if dropped > 0 {
            rendered =
                format!("（窗口超限，较早的 {dropped} 行工具结果 / 助手回复已省略）\n{rendered}");
        }

        Transcript {
            session: session.clone(),
            run: runs
                .last()
                .map(|w| w.run.clone())
                .unwrap_or_else(|| RunId::from_raw("")),
            rendered,
            cursors,
            user_chars,
            statuses: runs.iter().map(|w| (w.run.clone(), w.status)).collect(),
            statements: statements.join("\n"),
            roles,
            user_texts,
            injected,
            confined: None,
        }
    }

    /// 把这一批锁在一个作用域里（私有记忆的 Profile）。
    pub fn confined_to(mut self, scope: MemoryScope) -> Self {
        self.confined = Some(scope);
        self
    }

    /// 这一批被锁在哪个作用域；`None` = 不锁。
    pub fn confined(&self) -> Option<&MemoryScope> {
        self.confined.as_ref()
    }

    /// 这一批 Run 注入过的记忆引用。
    pub fn injected(&self) -> &[MemoryUse] {
        &self.injected
    }

    /// 把模型的 `used` 映射回记忆 id（§9.10）。`injected` 是这次调用里 `[m<n>]` 的编号表。
    ///
    /// 采纳 = 这一批的助手回复或工具调用**明显依赖**了它，所以证据必须引批内的助手事件
    /// （含它发起的工具调用）；用户行、单独的工具结果、不存在的 ID 都不算，整条丢弃。
    /// 编号规则同 §9.9：越界或非整数丢弃，同一编号只认第一条。
    pub fn validate_used(
        &self,
        raw: Vec<serde_json::Value>,
        injected: &[MemoryItem],
    ) -> Vec<MemoryId> {
        let mut seen = std::collections::HashSet::new();
        let mut adopted = Vec::new();
        for value in raw {
            let used: RawUse = match serde_json::from_value(value) {
                Ok(used) => used,
                Err(error) => {
                    tracing::warn!(%error, "used 读不懂，丢弃这一条");
                    continue;
                }
            };
            let index = match &used.memory {
                serde_json::Value::Number(number) => number.as_u64(),
                serde_json::Value::String(text) => {
                    let text = text.trim();
                    text.strip_prefix('m').unwrap_or(text).parse::<u64>().ok()
                }
                _ => None,
            };
            let Some((index, item)) = index
                .and_then(|index| usize::try_from(index).ok())
                .and_then(|index| injected.get(index).map(|item| (index, item)))
            else {
                tracing::warn!(memory = %used.memory, "used 里的编号不可用，丢弃这一条");
                continue;
            };
            let cites_assistant = used.evidence.iter().any(|reference| {
                matches!(
                    self.roles.get(reference.trim()),
                    Some((_, _, Role::Assistant))
                )
            });
            if !cites_assistant {
                tracing::warn!(index, "used 没有引批内的助手回复或工具调用，丢弃这一条");
                continue;
            }
            if !seen.insert(index) {
                tracing::warn!(index, "used 里同一个编号出现多次，只认第一条");
                continue;
            }
            adopted.push(item.id.clone());
        }
        adopted
    }

    /// 每个 Run 处理完推进到的 seq。
    pub fn run_cursors(&self) -> impl Iterator<Item = (&RunId, Seq)> {
        self.cursors.iter().map(|(run, seq)| (run, *seq))
    }

    /// 窗口里用户输入行的字符数。
    pub fn user_chars(&self) -> usize {
        self.user_chars
    }

    /// 这一批新发生的用户陈述（召回已有记忆的查询文本）。
    pub fn statements(&self) -> &str {
        &self.statements
    }

    /// 窗口字符数（渲染给模型的那一份）。
    pub fn window_chars(&self) -> usize {
        self.rendered.chars().count()
    }

    fn cursor(&self) -> Seq {
        self.cursors.values().copied().max().unwrap_or(Seq::ZERO)
    }

    /// 这条证据指的是这个窗口里的一句用户输入。
    pub fn cites_user(&self, reference: &EvidenceRef) -> bool {
        match reference {
            EvidenceRef::Event { session, event, .. } => {
                session == &self.session
                    && matches!(self.roles.get(event.as_str()), Some((_, _, Role::User)))
            }
            EvidenceRef::Memos { .. } => false,
        }
    }

    /// 这段对话里已经手动记过的那一条（`komo memory add`，§9.3）：证据引了窗口里的用户输入。
    pub fn recorded_by_hand(&self, item: &MemoryItem) -> bool {
        super::add::is_manual(item)
            && item
                .evidence
                .iter()
                .any(|evidence| self.cites_user(&evidence.reference))
    }

    /// 这个窗口里有没有**新发生的**用户陈述或可验证工具结果（§9.3）。
    pub fn has_new_evidence(&self) -> bool {
        self.roles
            .values()
            .any(|(_, _, role)| matches!(role, Role::User | Role::Tool))
            && !self.rendered.is_empty()
    }

    /// Rust 侧的校验（§9.3 那一步）。不合格的整条丢掉，并说清为什么。一批最多收
    /// `max_observations` 条。
    ///
    /// `candidates` 是这次提取调用的编号表（§9.9）：观察里的关系按它映射回 id。
    pub fn validate(
        &self,
        raw: Vec<RawObservation>,
        max_observations: usize,
        model: &ModelConfig,
        candidates: &[MemoryItem],
        now: OffsetDateTime,
    ) -> Vec<Observation> {
        let mut out = Vec::new();
        for candidate in raw.into_iter().take(max_observations) {
            match self.validate_one(candidate, model, candidates, now) {
                Ok(observation) => out.push(observation),
                Err(reason) => {
                    tracing::debug!(session = %self.session, %reason, "丢掉一条提取")
                }
            }
        }
        out
    }

    fn validate_one(
        &self,
        raw: RawObservation,
        model: &ModelConfig,
        candidates: &[MemoryItem],
        now: OffsetDateTime,
    ) -> Result<Observation, String> {
        let content = raw.content.trim().to_string();
        if content.is_empty() {
            return Err("正文是空的".into());
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(format!("正文 {} 字，太长了", content.chars().count()));
        }
        // 「自动整理不保存原始密钥、令牌或无关敏感输出」（§9.3）。
        if looks_like_secret(&content) {
            return Err("正文里像是有凭证".into());
        }

        let kind: MemoryKind = match raw.kind.as_str() {
            "preference" => MemoryKind::Preference,
            "experience" => MemoryKind::Experience,
            _ => MemoryKind::Fact,
        };
        // 锁定的批次（私有记忆）**不信模型给的作用域**：写进哪里由宿主定。没锁的批次里
        // 模型只能在 personal / project / environment 里选——`agent:<id>` 是私有记忆的
        // 专属格，模型写出来也不收。
        let scope: MemoryScope = match &self.confined {
            Some(scope) => scope.clone(),
            None if raw.scope.trim().is_empty() => MemoryScope::Personal,
            None => match raw.scope.parse() {
                Ok(MemoryScope::Agent { .. }) => {
                    return Err("agent 作用域不是模型能选的".into());
                }
                Ok(scope) => scope,
                Err(error) => return Err(format!("作用域不合法：{error}")),
            },
        };

        // 引用位置：每一条都要指向**这一批 Run 里真实存在的**事件。
        let mut evidence = Vec::new();
        let mut cited_user = false;
        let mut cited_tool = false;
        for reference in &raw.evidence {
            let Some((run, seq, role)) = self.roles.get(reference.trim()) else {
                continue;
            };
            // 「取消、失败或结果未知的动作不能整理成成功经验」（§9.3）：经验这一类只认
            // 已完成的 Run 里的证据。
            if kind == MemoryKind::Experience
                && self.statuses.get(run) != Some(&RunState::Completed)
            {
                continue;
            }
            match role {
                Role::User => cited_user = true,
                Role::Tool => cited_tool = true,
                // 助手摘要不算新的独立证据（§9.3）；任务交回的结果是任务 agent 写的摘要，
                // 同理（§9.10）。所以它们连证据都不记。
                Role::Assistant | Role::TaskResult => continue,
            }
            evidence.push(Evidence {
                reference: EvidenceRef::Event {
                    session: self.session.clone(),
                    event: EventId::from_raw(reference.trim()),
                    seq: *seq,
                },
                provenance: match role {
                    Role::User => Provenance::UserStatement,
                    _ => Provenance::ToolObservation,
                },
                observed_at: now,
                extracted_from_run: Some(run.clone()),
            });
        }
        if evidence.is_empty() {
            // 「提取来源限定为新发生的用户陈述和可验证工具结果」（§9.3）——一条谁都不
            // 引、或者只引助手自己的话的观察，没有来源可言。
            return Err("没有指向用户陈述或工具结果的证据".into());
        }

        // **来源角色由证据决定，不由模型自称决定。**模型说"这是用户说的"而只引得出工具
        // 结果时，它就是一条推断——降级，而不是相信它。
        let provenance = match raw.said_by.as_provenance() {
            Provenance::UserStatement if cited_user => Provenance::UserStatement,
            Provenance::ToolObservation if cited_tool => Provenance::ToolObservation,
            Provenance::ModelInference => Provenance::ModelInference,
            // 自称对不上证据 → 按推断收，进候选池等人看（§9.2）。
            _ => Provenance::ModelInference,
        };

        // 「明确的用户原话可标为 active + user_statement + unconfirmed…模型推断默认
        // candidate，不作为已确认事实注入」（§9.2）。
        let state = match provenance {
            Provenance::UserStatement | Provenance::ToolObservation => MemoryState::Active,
            Provenance::ModelInference => MemoryState::Candidate,
        };

        let observed_at = raw
            .observed_at
            .as_deref()
            .and_then(parse_time)
            // 事实的观察时间区别于入库时间（§9.2）；模型没说就用证据的时间。
            .unwrap_or_else(|| evidence.first().map(|e| e.observed_at).unwrap_or(now));
        let valid_until = raw.valid_until.as_deref().and_then(parse_time);

        let (application, applies_when) = self.classify(&raw, kind, provenance, &evidence);

        let mut extraction =
            ExtractionMetadata::new(model.model.clone(), model.effort.clone(), PROMPT_VERSION);
        extraction.source_cursor = Some(self.cursor());

        let relations = map_verdicts(
            raw.relations
                .into_iter()
                .filter_map(|value| match serde_json::from_value::<RawVerdict>(value) {
                    Ok(verdict) => Some(verdict),
                    Err(error) => {
                        tracing::warn!(%error, "关系读不懂，丢弃这一条");
                        None
                    }
                })
                .collect(),
            candidates,
        );

        Ok(Observation {
            content,
            kind,
            scope,
            provenance,
            state,
            evidence,
            observed_at,
            valid_until,
            extraction,
            relations,
            application: Some(application),
            applies_when,
        })
    }

    /// 使用方式的校验（§9.3）：convention 必须同时满足——kind 是 preference、模型自称与
    /// 证据都是用户陈述、`applies_when` 非空、`basis` 逐字出现在所引的某条用户事件里（空白
    /// 归一后比较）。任何一条不满足就降为 reference：观察照常落库，只是不当约定。
    ///
    /// 原话匹配只证明引用真实，不证明语义理解对；这一步挡的是"工具观察 / 推断冒充约定"
    /// 和"编一句原话"，长期性本身靠提示词把关。
    fn classify(
        &self,
        raw: &RawObservation,
        kind: MemoryKind,
        provenance: Provenance,
        evidence: &[Evidence],
    ) -> (MemoryApplication, Option<String>) {
        let applies_when = raw
            .applies_when
            .as_deref()
            .map(str::trim)
            .filter(|when| !when.is_empty() && when.chars().count() <= MAX_CONTENT_CHARS)
            .map(str::to_string);
        if raw.application.as_deref().map(str::trim) != Some("convention") {
            return (MemoryApplication::Reference, applies_when);
        }
        let basis = raw
            .basis
            .as_deref()
            .map(squash_whitespace)
            .unwrap_or_default();
        let quoted = !basis.is_empty()
            && evidence.iter().any(|evidence| match &evidence.reference {
                EvidenceRef::Event { event, .. } => self
                    .user_texts
                    .get(event.as_str())
                    .is_some_and(|text| text.contains(&basis)),
                EvidenceRef::Memos { .. } => false,
            });
        let qualifies = kind == MemoryKind::Preference
            && raw.said_by == SaidBy::UserStatement
            && provenance == Provenance::UserStatement
            && applies_when.is_some()
            && quoted;
        if qualifies {
            (MemoryApplication::Convention, applies_when)
        } else {
            tracing::debug!(
                session = %self.session,
                "convention 校验没过（preference / 用户陈述 / 条件 / basis 原话），降为 reference"
            );
            (MemoryApplication::Reference, applies_when)
        }
    }
}

/// 空白归一：连续空白（含换行）折成一个空格并去掉首尾，basis 与原文都按它比较。
fn squash_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 把一批 Run 按受理顺序切成几段，每段的用户输入行加起来不超窗口预算（§9.10）。
/// 常态是一段；单个 Run 自己就超预算时它独占一段（用户行照样全留）。
pub fn split_batch(
    session: &SessionId,
    runs: &[RunWindow],
    events: &[Event],
) -> Vec<Vec<RunWindow>> {
    let mut segments: Vec<Vec<RunWindow>> = Vec::new();
    let mut used = 0usize;
    for window in runs {
        let chars = Transcript::build(session, std::slice::from_ref(window), events).user_chars();
        match segments.last_mut() {
            Some(current) if used + chars <= MAX_TRANSCRIPT_CHARS => {
                current.push(window.clone());
                used += chars;
            }
            _ => {
                segments.push(vec![window.clone()]);
                used = chars;
            }
        }
    }
    segments
}

fn label(role: Role) -> &'static str {
    match role {
        Role::User => "用户",
        Role::TaskResult => "任务交回",
        Role::Tool => "工具结果",
        Role::Assistant => "助手",
    }
}

/// 一次工具调用的短摘要：工具名加压成一行、截到 200 字的参数。
fn call_summary(call: &ToolCallRequest) -> String {
    if call.arguments.is_null() && call.arguments_ref.is_some() {
        return format!("{} （参数外置）", call.name);
    }
    let arguments = call.arguments.to_string().replace(['\n', '\r'], " ");
    if arguments.chars().count() > MAX_CALL_ARGUMENT_CHARS {
        let head: String = arguments.chars().take(MAX_CALL_ARGUMENT_CHARS).collect();
        format!("{} {head}…", call.name)
    } else {
        format!("{} {arguments}", call.name)
    }
}

fn one_line(text: &str) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    if flat.chars().count() > 600 {
        flat.chars().take(600).collect::<String>() + "…"
    } else {
        flat
    }
}

fn parse_time(raw: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).ok()
}

/// 粗筛"看起来像凭证"的正文。宁可丢掉一条记忆，也不把一把密钥存成长期事实（§9.3）。
pub(super) fn looks_like_secret(text: &str) -> bool {
    let lower = text.to_lowercase();
    const MARKERS: [&str; 6] = [
        "sk-",
        "bearer ",
        "-----begin",
        "api_key=",
        "apikey=",
        "password=",
    ];
    if MARKERS.iter().any(|marker| lower.contains(marker)) {
        return true;
    }
    // 一长串十六进制 / base64 样的东西。
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| word.len() >= 32 && word.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// 提取这一步本身：一次记忆模型往返，**不给工具**。
pub struct LearningPass {
    llm: Arc<dyn LlmClient>,
    model: ModelConfig,
    clock: Arc<dyn Clock>,
    /// 一批最多收几条（提示词里要告诉模型；超出的 [`Transcript::validate`] 也会截掉）。
    max_observations: usize,
}

impl LearningPass {
    pub fn new(
        llm: Arc<dyn LlmClient>,
        model: ModelConfig,
        clock: Arc<dyn Clock>,
        max_observations: usize,
    ) -> Self {
        LearningPass {
            llm,
            model,
            clock,
            max_observations,
        }
    }

    /// 发一次请求，把 JSON 解出来。**解不出来是错误，不是空结果**——"提取失败"和"这一
    /// 轮没有可记的东西"是两件事，后者会推进游标而前者要重试（§9.3）。
    ///
    /// `candidates` 是提取前召回的已有记忆，按下标编号放进提示词（§9.9、§9.10）。
    pub async fn extract(
        &self,
        transcript: &Transcript,
        candidates: &[MemoryItem],
        injected: &[MemoryItem],
    ) -> Result<RawExtraction, MemoryError> {
        let request = TurnRequest {
            session: transcript.session.clone(),
            run: transcript.run.clone(),
            // **记忆模型自己的那份完整配置**（§13.3）：`RoutingLlm` 按它挑实例，所以
            // 切换聊天模型或它的 effort 碰不到这一次请求。
            model: self.model.clone(),
            system_prompt: system_prompt(self.max_observations, transcript.confined()),
            messages: vec![komo_kernel::types::turn::ReplayMessage {
                role: komo_kernel::types::turn::Role::User,
                seq: transcript.cursor(),
                text: Some(user_prompt(
                    transcript,
                    candidates,
                    injected,
                    self.clock.now(),
                )),
                tool_calls: vec![],
                tool_results: vec![],
                provider_blocks: None,
            }],
            // 「记忆模型提取结构化候选，**不提供执行工具**」（§9.3）。
            tools: vec![],
            memories: vec![],
            covers: None,
            cache_session: None,
        };

        let mut driver = self.llm.begin_turn(request).await?;
        let round = crate::telemetry::turn(
            &mut *driver,
            RoundInput::First,
            crate::telemetry::role::MEMORY_EXTRACT,
            &self.model,
            1,
        )
        .await?;
        if round.truncated {
            return Err(MemoryError::Invalid("回复被截断，这一轮不采信".into()));
        }
        let text = round.text.unwrap_or_default();
        parse_extraction(&text)
    }
}

/// 从模型正文里解出观察列表。容忍 ```json 围栏与前后的闲话。
pub fn parse_extraction(text: &str) -> Result<RawExtraction, MemoryError> {
    let Some(json) = parse_json_object(text) else {
        return Err(MemoryError::Invalid(format!(
            "回复里找不到 JSON 对象：{}",
            one_line(&text.chars().take(200).collect::<String>())
        )));
    };
    serde_json::from_str(json)
        .map_err(|error| MemoryError::Invalid(format!("JSON 解不开：{error}")))
}

/// 剥掉围栏、取出第一个配平的 JSON 对象。提取与关系判断共用它——两边容忍的东西不同，
/// 迟早会有一边的括号处理少一个分支。
pub fn parse_json_object(text: &str) -> Option<&str> {
    first_object(strip_fence(text))
}

fn strip_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    rest.trim_start_matches('\n')
        .strip_suffix("```")
        .unwrap_or(rest)
        .trim()
}

/// 第一个配平的 `{…}`。字符串里的括号不算。
fn first_object(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=index]);
                }
            }
            _ => {}
        }
    }
    None
}

/// 提取用的系统提示。
///
/// 三句是硬的，因为它们各自对应一条 Rust 侧的闸：只提新发生的用户陈述与可验证工具结果
/// （§9.3）；每条必须引事件 ID（校验引用位置）；不要报告确认状态（§9.2 那句"模型返回的
/// user_confirmed 字段没有写入权限"——提示里说清楚，省得模型白写一遍）。
///
/// 私有记忆的批次（`confined`）把允许的作用域收窄成它那一个：模型只被告知这一个。
fn system_prompt(max_observations: usize, confined: Option<&MemoryScope>) -> String {
    let prompt = SYSTEM_PROMPT.replace("{MAX}", &max_observations.to_string());
    match confined {
        Some(scope) => prompt.replace("personal|project:<id>|environment:<id>", &scope.to_string()),
        None => prompt,
    }
}

const SYSTEM_PROMPT: &str = "\
你在为一个个人助手整理长期记忆。读一段会话记录（可能包含这个会话里的多个任务，按发生顺序排列），挑出**值得长期记住**的偏好、项目事实和\
执行经验，用 JSON 交回来。

只输出一个 JSON 对象，形如：
{\"observations\":[{\"content\":\"简短的单句陈述\",\"kind\":\"preference|fact|experience\",\
\"scope\":\"personal|project:<id>|environment:<id>\",\
\"said_by\":\"user_statement|tool_observation|model_inference\",\
\"evidence\":[\"<事件 ID>\"],\"observed_at\":\"RFC3339，可省略\",\
\"valid_until\":\"RFC3339，可省略\",\
\"application\":\"convention|reference\",\"applies_when\":\"条件，convention 必填\",\
\"basis\":\"仅 convention：所引用户事件里的原话片段\",\
\"relations\":[{\"memory\":<已有记忆方括号里的编号，整数>,\"relation\":\
\"same|supports|contradicts|supersedes|unrelated\"}]}],\
\"used\":[{\"memory\":\"<注入记忆方括号里的编号，如 m0>\",\"evidence\":[\"<助手事件 ID>\"]}]}
（used 只在记录里列了「本批注入的记忆」时才写，否则省略。）

规则：
1. 每条都必须在 evidence 里引至少一个**用户**或**工具结果**事件的 ID，原样照抄方括号里\
那个 ID。引不出来就不要提这一条。标成「任务交回」的是后台任务、监听或后台命令交回的内容，**不是用户说的\
话**，不能引用它，也不能把它当成 user_statement。
2. said_by 如实说：用户原话是 user_statement，工具结果是 tool_observation，你自己从中\
推断出来的是 model_inference。不要把推断说成用户原话。
3. content 是一句独立成立的陈述，短，只说一件事，不含代词指代（「他」「那个」要展开）。\
易变的现场状态（此刻的开关、此刻的天气）不是长期事实，不要提。
4. 只记**以后的对话还用得上**的东西。一次性的操作流水（「删除了 9 个文件」「已重启某服务」\
「把某配置改成了 X」）、这一轮的进度与结果都不是记忆；从中得出的可复用结论（某命令怎么用、\
某设备该怎么查）才是。
5. **随时能重新读到的不记。**工具结果前面写着是哪次调用：读文件、搜代码、看 --help 读到的\
文档、源码、配置、skill 说明、命令用法，原样摘一句不是记忆——下次再读一遍就有，而且比记忆新。\
值得记的是**翻不到**的东西：用户说的偏好、约定和背景；排查、试错之后才得出的结论；文档没写、\
踩过才知道的坑。拿不准时，问自己「不记这条，下次是否要重新排查或让用户再说一遍」，否则不记。
6. 宁缺毋滥：一次最多交回 {MAX} 条，挑复用价值最高的；同一件事只写一条。
7. 不要写入任何密钥、令牌、密码或与任务无关的敏感输出。
8. 不要报告确认状态或生命周期——那不是你的字段。
9. 没有值得记的就交回 {\"observations\":[]}。这是常见情况。不要编。
10. 每条观察的 relations 说明它与「已有的相关记忆」里哪几条有关系，可以省略或留空。只写有关系的，\
编号只能用方括号里的整数，不要抄内容。same = 同一件事换了说法；supports = 不同主张但支持它；\
contradicts = 实质冲突，两者不能同时为真；supersedes = 同一个问题的新答案，旧的已经不成立；\
unrelated = 不相干（可不写）。判冲突前先看作用域和时间：不同项目的偏好不是冲突，不同时间发生的同类\
事件（比如两次更换滤芯）也不是冲突，各自保留。只与同作用域的条目建立关系。
11. used 说明「本批注入的记忆」里哪几条被**用上**了：助手的回复或工具调用明显依赖了它，没有它\
就会做得不一样（照着它选了参数、遵守了它说的偏好、引用了它的事实）。只是话题相关、助手复述了\
一遍、或者根本没体现，都不算。evidence 引批内助手事件的 ID（工具调用在发起它的助手事件上），不能\
引用户行或单独的工具结果。没有就交回空数组；不要凑。
12. 「已有的相关记忆」里标着「本段已手动记下」的，是这段对话里已经用 `komo memory add` 记过的：\
同一件事不要再交回，换个说法也不要；同一句话里说到的**别的**值得记的事照常提。
13. 每条都要定 application，先判**长期性**再定使用方式：
- convention = 用户表达了长期意图（「以后」「默认」「每次都」「一律」「别再」这类话）的偏好或限制。\
只能是 kind=preference、said_by=user_statement；applies_when 必填，写成一句能独立理解的条件\
（「创建发布工单、需要选 Platform 时」）；basis 必填，从你引的**用户**事件里原样摘出表达长期意图与\
条件的那句话，不要改写。
- reference = 其余一切：事实、排查得来的经验、工具观察、你的推断，以及拿不准长期性或条件的用户话。\
applies_when 可省。拿不准时一律交回 reference，不要硬标 convention。
- **当次要求不是记忆**：「这个工单选 saas」「这次优化用 terra 子 Agent 并行做」只管眼前的任务，\
不要提。工具结果显示上次选了 saas，通常也不提；提也只能是 reference，推不出用户的默认值。
示例：用户说「以后发布工单 Platform 都选 saas」→ preference + convention，applies_when=「创建发布\
工单、需要选 Platform 时」，basis=「以后发布工单 Platform 都选 saas」；用户说「ob-note 这个仓库的 Git \
设置别改全局」→ preference + convention，scope 用该项目，applies_when=「在 ob-note 仓库里改 Git \
设置时」；排查证明某接口要 `issueKey` → experience + reference；你猜用户喜欢 saas → \
model_inference + reference。";

/// 编号候选行里的使用方式与条件（§9.6：判关系要看它们——正文相同、条件不同的是两条）。
/// 旧条目没分类，如实写「未分类」，不替它说成 reference。
pub(super) fn application_label(item: &MemoryItem) -> String {
    let application = item
        .application
        .map(|a| a.as_str().to_string())
        .unwrap_or_else(|| "未分类".to_string());
    match item.applies_when.as_deref() {
        Some(when) => format!(" 使用方式={application} 条件：{when}"),
        None => format!(" 使用方式={application}"),
    }
}

fn user_prompt(
    transcript: &Transcript,
    candidates: &[MemoryItem],
    injected: &[MemoryItem],
    now: OffsetDateTime,
) -> String {
    // 编号落在自己的命名空间（m0、m1…），与上面候选的 [0]、[1] 不混。
    let injected = if injected.is_empty() {
        String::new()
    } else {
        let lines = injected
            .iter()
            .enumerate()
            .map(|(index, item)| format!("[m{index}] {}", item.content))
            .collect::<Vec<_>>()
            .join("\n");
        format!("--- 本批注入的记忆 ---\n{lines}\n--- 注入结束 ---\n\n")
    };
    let existing = if candidates.is_empty() {
        "（没有）".to_string()
    } else {
        candidates
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let by_hand = if transcript.recorded_by_hand(item) {
                    " 本段已手动记下"
                } else {
                    ""
                };
                format!(
                    "[{index}] 状态={:?} 来源={:?} 确认={:?} 作用域={}{by_hand}{} 内容：{}",
                    item.state,
                    item.provenance,
                    item.confirmation,
                    item.scope,
                    application_label(item),
                    item.content
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "当前时间：{}\n会话：{}\n\n--- 已有的相关记忆 ---\n{existing}\n--- 记忆结束 ---\n\n{injected}\
         --- 会话记录 ---\n{}\n--- 记录结束 ---",
        now.date(),
        transcript.session,
        transcript.rendered
    )
}
