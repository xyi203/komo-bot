//! LearningPass：一个已完成 Run 的会话窗口 → 结构化观察（§9.3）。
//!
//! 「记忆模型提取结构化候选，**不提供执行工具** → **Rust 校验**来源角色、引用位置、
//! 作用域与内容」——这两步在这个文件里是两段代码，中间没有捷径：
//!
//! - [`LearningPass::extract`] 只负责发一次请求、把 JSON 解出来。它交出的是
//!   [`RawObservation`]，一个**模型说了什么**的记录，还不是一条记忆。
//! - [`Transcript::validate`] 把 raw 变成 [`Observation`]。这里落三条硬规则：
//!   证据必须指向**这个 Run 里真实存在的**用户消息或工具结果；来源角色由证据决定而不
//!   是由模型自称决定；`user_confirmed` 根本没有被反序列化进来——模型返回的那个字段
//!   没有写入权限（§9.2），做法是这组类型里没有它的位置。

use std::collections::BTreeMap;
use std::sync::Arc;

use komo_kernel::events::{Event, EventPayload};
use komo_kernel::traits::{Clock, LlmClient};
use komo_kernel::types::ids::{EventId, RunId, Seq, SessionId};
use komo_kernel::types::memory::{
    Confirmation, Evidence, EvidenceRef, ExtractionMetadata, MemoryKind, MemoryScope, MemoryState,
    Provenance,
};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::status::RunStatus;
use komo_kernel::types::turn::{RoundInput, TurnRequest};
use serde::Deserialize;
use time::OffsetDateTime;

use super::{MemoryError, MemoryWorkItem};

/// 提示词版本。**改了提示词就改它**——它进 [`ExtractionMetadata`]，是"这条记忆是怎么来
/// 的"里唯一能回答"按哪一版规则提的"的那一格（§9.2）。
pub const PROMPT_VERSION: &str = "memory-extract/v1";

/// 一条记忆正文的上限。再长就不是"简短、尽量单一的陈述"了（§9.2）。
const MAX_CONTENT_CHARS: usize = 300;
/// 一次最多接受几条观察。模型话多不该变成库里一次涨一百条。
const MAX_OBSERVATIONS: usize = 12;
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
    /// 证据引用：这个 Run 的事件 ID。
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub observed_at: Option<String>,
    #[serde(default)]
    pub valid_until: Option<String>,
}

fn default_kind() -> String {
    "fact".into()
}

#[derive(Debug, Deserialize)]
struct Extraction {
    #[serde(default)]
    observations: Vec<RawObservation>,
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
}

impl Observation {
    /// 入库时的确认等级。**永远是 `Unconfirmed`**：只有操作者的 `confirm` 抬得起来
    /// （§9.2），而这个函数是提取这条路上唯一填它的地方。
    pub fn confirmation(&self) -> Confirmation {
        Confirmation::Unconfirmed
    }
}

/// 这个 Run 的会话窗口，连同"哪条事件是谁说的"的索引。
#[derive(Debug, Clone)]
pub struct Transcript {
    pub session: SessionId,
    pub run: RunId,
    /// 渲染给记忆模型看的正文。
    pub rendered: String,
    /// 处理到哪条 seq（处理完推进到这里）。
    pub cursor: Seq,
    /// 事件 ID → （seq，角色）。**校验引用位置**用的就是它。
    roles: BTreeMap<String, (Seq, Role)>,
}

/// 一条事件在提取里算什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// 用户陈述。
    User,
    /// 可验证的工具结果。
    Tool,
    /// 助手自己说的话。**不算新的独立证据**（§9.3）。
    Assistant,
}

impl Transcript {
    /// 把一个 Run 的事件渲染成窗口。`cursor` 之前的不再算新证据（§9.3 的来源游标）。
    pub fn build(session: &SessionId, run: &RunId, events: &[Event], cursor: Seq) -> Transcript {
        let mut roles = BTreeMap::new();
        let mut lines: Vec<String> = Vec::new();
        let mut highest = cursor;

        for event in events {
            // 只看这个 Run 的证据。别的 Run 有它自己的一次处理。
            if event.run.as_ref() != Some(run) {
                continue;
            }
            highest = highest.max(event.seq);
            let fresh = event.seq > cursor;
            let (role, body) = match &event.payload {
                EventPayload::RunAccepted(body) => (
                    Role::User,
                    body.text.clone().unwrap_or_else(|| "（正文外置）".into()),
                ),
                EventPayload::MessageUser(body) => (
                    Role::User,
                    body.text.clone().unwrap_or_else(|| "（正文外置）".into()),
                ),
                EventPayload::MessageAssistant(body) => (
                    Role::Assistant,
                    body.text
                        .clone()
                        .unwrap_or_else(|| "（只有工具调用）".into()),
                ),
                EventPayload::ToolResult(body) => (
                    Role::Tool,
                    format!(
                        "{:?} · {}",
                        body.status,
                        body.preview.clone().unwrap_or_else(|| "（无预览）".into())
                    ),
                ),
                _ => continue,
            };
            roles.insert(event.event_id.to_string(), (event.seq, role));
            if !fresh {
                // 旧证据仍然渲染给模型当上下文，但标出来——它不是新发生的事。
                lines.push(format!(
                    "[{} seq={} {} · 已处理过]{}",
                    event.event_id,
                    event.seq,
                    label(role),
                    one_line(&body)
                ));
                continue;
            }
            lines.push(format!(
                "[{} seq={} {}] {}",
                event.event_id,
                event.seq,
                label(role),
                one_line(&body)
            ));
        }

        let mut rendered = lines.join("\n");
        if rendered.chars().count() > MAX_TRANSCRIPT_CHARS {
            // 从后往前留：最近发生的才是"新证据"。
            let keep: String = rendered
                .chars()
                .rev()
                .take(MAX_TRANSCRIPT_CHARS)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            rendered = format!("（前面的窗口已截断）\n{keep}");
        }

        Transcript {
            session: session.clone(),
            run: run.clone(),
            rendered,
            cursor: highest,
            roles,
        }
    }

    /// 这个窗口里有没有**新发生的**用户陈述或可验证工具结果（§9.3）。
    pub fn has_new_evidence(&self) -> bool {
        self.roles
            .values()
            .any(|(_, role)| matches!(role, Role::User | Role::Tool))
            && !self.rendered.is_empty()
    }

    /// Rust 侧的校验（§9.3 那一步）。不合格的整条丢掉，并说清为什么。
    pub fn validate(
        &self,
        raw: Vec<RawObservation>,
        item: &MemoryWorkItem,
        model: &ModelConfig,
        now: OffsetDateTime,
    ) -> Vec<Observation> {
        let mut out = Vec::new();
        for candidate in raw.into_iter().take(MAX_OBSERVATIONS) {
            match self.validate_one(candidate, item, model, now) {
                Ok(observation) => out.push(observation),
                Err(reason) => tracing::debug!(run = %self.run, %reason, "丢掉一条提取"),
            }
        }
        out
    }

    fn validate_one(
        &self,
        raw: RawObservation,
        item: &MemoryWorkItem,
        model: &ModelConfig,
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
        let scope: MemoryScope = if raw.scope.trim().is_empty() {
            MemoryScope::Personal
        } else {
            raw.scope
                .parse()
                .map_err(|error| format!("作用域不合法：{error}"))?
        };

        // 「取消、失败或结果未知的动作不能整理成成功经验」（§9.3）。取消的 Run 整个不
        // 进来；失败的 Run 里，**经验**这一类没有成立的基础。
        if kind == MemoryKind::Experience && item.status != RunStatus::Completed {
            return Err(format!("{:?} 的 Run 整理不出成功经验", item.status));
        }
        // 「Cron 只在任务授予的范围内积累有证据的事实和经验，**不能从自己的报告推断用户
        // 偏好**」（§9.3）。
        if kind == MemoryKind::Preference && matches!(item.source, PlanSource::Cron { .. }) {
            return Err("Cron 的 Run 不整理用户偏好".into());
        }

        // 引用位置：每一条都要指向**这个 Run 里真实存在的**事件。
        let mut evidence = Vec::new();
        let mut cited_user = false;
        let mut cited_tool = false;
        for reference in &raw.evidence {
            let Some((seq, role)) = self.roles.get(reference.trim()) else {
                continue;
            };
            match role {
                Role::User => cited_user = true,
                Role::Tool => cited_tool = true,
                // 助手摘要不算新的独立证据（§9.3），所以它连证据都不记。
                Role::Assistant => continue,
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
                extracted_from_run: Some(self.run.clone()),
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

        let mut extraction =
            ExtractionMetadata::new(model.model.clone(), model.effort.clone(), PROMPT_VERSION);
        extraction.source_cursor = Some(self.cursor);

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
        })
    }
}

fn label(role: Role) -> &'static str {
    match role {
        Role::User => "用户",
        Role::Tool => "工具结果",
        Role::Assistant => "助手",
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
fn looks_like_secret(text: &str) -> bool {
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
}

impl LearningPass {
    pub fn new(llm: Arc<dyn LlmClient>, model: ModelConfig, clock: Arc<dyn Clock>) -> Self {
        LearningPass { llm, model, clock }
    }

    /// 发一次请求，把 JSON 解出来。**解不出来是错误，不是空结果**——"提取失败"和"这一
    /// 轮没有可记的东西"是两件事，后者会推进游标而前者要重试（§9.3）。
    pub async fn extract(
        &self,
        transcript: &Transcript,
    ) -> Result<Vec<RawObservation>, MemoryError> {
        let request = TurnRequest {
            session: transcript.session.clone(),
            run: transcript.run.clone(),
            // **记忆模型自己的那份完整配置**（§13.3）：`RoutingLlm` 按它挑实例，所以
            // 切换聊天模型或它的 effort 碰不到这一次请求。
            model: self.model.clone(),
            system_prompt: SYSTEM_PROMPT.to_string(),
            messages: vec![komo_kernel::types::turn::ReplayMessage {
                role: komo_kernel::types::turn::Role::User,
                seq: transcript.cursor,
                text: Some(user_prompt(transcript, self.clock.now())),
                tool_calls: vec![],
                tool_results: vec![],
                provider_blocks: None,
            }],
            // 「记忆模型提取结构化候选，**不提供执行工具**」（§9.3）。
            tools: vec![],
            memories: vec![],
            covers: None,
        };

        let mut driver = self.llm.begin_turn(request).await?;
        let round = driver.next(RoundInput::First).await?;
        if round.truncated {
            return Err(MemoryError::Invalid("回复被截断，这一轮不采信".into()));
        }
        let text = round.text.unwrap_or_default();
        parse_extraction(&text)
    }
}

/// 从模型正文里解出观察列表。容忍 ```json 围栏与前后的闲话。
pub fn parse_extraction(text: &str) -> Result<Vec<RawObservation>, MemoryError> {
    let Some(json) = parse_json_object(text) else {
        return Err(MemoryError::Invalid(format!(
            "回复里找不到 JSON 对象：{}",
            one_line(&text.chars().take(200).collect::<String>())
        )));
    };
    let parsed: Extraction = serde_json::from_str(json)
        .map_err(|error| MemoryError::Invalid(format!("JSON 解不开：{error}")))?;
    Ok(parsed.observations)
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
const SYSTEM_PROMPT: &str = "\
你在为一个个人助手整理长期记忆。读一段会话记录，挑出**值得长期记住**的偏好、项目事实和\
执行经验，用 JSON 交回来。

只输出一个 JSON 对象，形如：
{\"observations\":[{\"content\":\"简短的单句陈述\",\"kind\":\"preference|fact|experience\",\
\"scope\":\"personal|project:<id>|environment:<id>\",\
\"said_by\":\"user_statement|tool_observation|model_inference\",\
\"evidence\":[\"<事件 ID>\"],\"observed_at\":\"RFC3339，可省略\",\
\"valid_until\":\"RFC3339，可省略\"}]}

规则：
1. 每条都必须在 evidence 里引至少一个**用户**或**工具结果**事件的 ID，原样照抄方括号里\
那个 ID。引不出来就不要提这一条。
2. said_by 如实说：用户原话是 user_statement，工具结果是 tool_observation，你自己从中\
推断出来的是 model_inference。不要把推断说成用户原话。
3. content 是一句独立成立的陈述，短，只说一件事，不含代词指代（「他」「那个」要展开）。\
易变的现场状态（此刻的开关、此刻的天气）不是长期事实，不要提。
4. 不要写入任何密钥、令牌、密码或与任务无关的敏感输出。
5. 不要报告确认状态或生命周期——那不是你的字段。
6. 没有值得记的就交回 {\"observations\":[]}。不要编。";

fn user_prompt(transcript: &Transcript, now: OffsetDateTime) -> String {
    format!(
        "当前时间：{}\n会话：{}\n任务：{}\n\n--- 会话记录 ---\n{}\n--- 记录结束 ---",
        now.date(),
        transcript.session,
        transcript.run,
        transcript.rendered
    )
}
