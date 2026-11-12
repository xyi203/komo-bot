//! Router 的分发判断（`docs/tasks.md` §4.11、§4.12 U4/U5、§4.13、§4.14 F2/F5）。
//!
//! Router 不是会话：每条没有直通去处的消息触发一次模型调用，只填一个固定格式的去向。这里是
//! "拼好的输入 → 提示 → 去向"这个单元，**不读存储、不建任务、不投递**：
//!
//! - [`RouterInput::prompt`] / [`RouterInput::interpret`] / [`RouterInput::fallback`] 是三段纯函数，
//!   Gateway 把"判断"放进引擎任务的检查点时可以单独用，重做没有副作用；
//! - [`route`] 把三段串起来（调模型、校验、重试到上限、兜底），调用方拿到的**永远是一个合法
//!   去向**，外加"是不是兜底得来的、为什么"的标记（[`Routed::fallback`]）供记录。
//!
//! 模型调用是注入的 [`RouterModel`]：komo-agent 只依赖 kernel，不碰 `komo-llm`。
//!
//! 模型回的格式是一个 JSON 对象（见 [`context::router`] 的提示），解析前先剥掉外面可能包着的
//! 代码围栏与前后说明文字。`komo-llm` 的 `json_repair` 是 crate 私有的、且依赖方向不允许
//! agent 去用它，所以这里只做"取出第一个能解析的 JSON 对象"，不修复残缺的 JSON——残缺就算
//! 答案不合格式，重试或兜底。
//!
//! [`context::router`]: crate::context::router

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::context::router::RouterPrompt;
use crate::context::router::render;

/// 一次判断最多调几次模型（含第一次）。判断在每条消息的入口上，不能无限重试；耗尽就兜底。
pub const MAX_ATTEMPTS: u32 = 2;
/// 带几条最近的消息（私聊是主线上的，群是这个话题或群里的）。`docs/tasks.md` §4.11：先取 6。
pub const DEFAULT_RECENT_MESSAGES: usize = 6;
/// 带几个最近结束的任务。`docs/tasks.md` §4.11 决定 2：先取 5。
pub const DEFAULT_FINISHED_TASKS: usize = 5;
/// 任务标题最多这么多个字符（模型给的与兜底从原文截的都按它）。
pub const TITLE_CHARS: usize = 60;

/// 这条消息从哪来。决定能用哪些去向，也决定候选从哪个范围取（调用方的事）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouteScope {
    /// 操作者私聊：有 `list`，没有 `ignore`；判断失败按 `new`。
    Private,
    /// 群：有 `ignore`，没有 `list`；判断失败按 `ignore`。
    Group,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateState {
    Running,
    /// 等用户回话（有未答的介入，或受阻）。
    Waiting,
    /// 最近结束的。
    Finished,
}

/// 候选清单里的一项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateTask {
    /// 任务标识：只在程序与 Router 之间用（U5）。
    pub id: String,
    pub title: String,
    pub state: CandidateState,
    /// 最后一次往外说的话。渲染时截断到 [`crate::context::router::LAST_SAID_CHARS`]。
    pub last_said: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Speaker {
    Operator,
    Agent,
    /// 群里别的人。
    Member,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentMessage {
    pub speaker: Speaker,
    pub text: String,
}

/// 最近消息、最近结束任务各带几条。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouterLimits {
    pub recent_messages: usize,
    pub finished_tasks: usize,
}

impl Default for RouterLimits {
    fn default() -> Self {
        RouterLimits {
            recent_messages: DEFAULT_RECENT_MESSAGES,
            finished_tasks: DEFAULT_FINISHED_TASKS,
        }
    }
}

/// 一次判断的全部输入：程序拼好，没有对话历史、没有工具。
///
/// 怎样从存储里读出这些是调用方的事；群里调用方只给**这个群**的任务与消息
/// （`docs/tasks.md` §4.13：不带操作者私聊的清单）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterInput {
    pub scope: RouteScope,
    /// 这条消息的原文。
    pub message: String,
    /// 候选任务：进行中与等用户的在前，最近结束的在后。`continue` / `stop` 只能指这里面的。
    pub tasks: Vec<CandidateTask>,
    /// 最近的消息，旧的在前。
    pub recent: Vec<RecentMessage>,
}

impl RouterInput {
    /// 按上限收拢：进行中与等用户的全留；已结束的只留 `limits.finished_tasks` 个（`tasks` 里
    /// 已结束的**新的在前**）；最近消息只留最后 `limits.recent_messages` 条（`recent` 旧的在前）。
    pub fn new(
        scope: RouteScope,
        message: impl Into<String>,
        tasks: Vec<CandidateTask>,
        recent: Vec<RecentMessage>,
        limits: RouterLimits,
    ) -> Self {
        let (mut live, finished): (Vec<_>, Vec<_>) = tasks
            .into_iter()
            .partition(|task| task.state != CandidateState::Finished);
        live.extend(finished.into_iter().take(limits.finished_tasks));
        let skip = recent.len().saturating_sub(limits.recent_messages);
        RouterInput {
            scope,
            message: message.into(),
            tasks: live,
            recent: recent.into_iter().skip(skip).collect(),
        }
    }

    /// 渲染提示。纯函数。
    pub fn prompt(&self) -> RouterPrompt {
        render(self)
    }

    /// 把模型的一段回复解释成去向；不合格式、去向不属于这个范围、任务标识不在候选里都是失败。
    pub fn interpret(&self, text: &str) -> Result<Route, RouteFailure> {
        let object = json_object(text)
            .ok_or_else(|| RouteFailure::Malformed("回复里没有能解析的 JSON 对象".into()))?;
        let field = |name: &str| object.get(name).and_then(Value::as_str).map(str::trim);
        let kind = field("route")
            .ok_or_else(|| RouteFailure::Malformed("缺少 route 字段".into()))?
            .to_ascii_lowercase();
        let known_task = |object_field: &str| -> Result<String, RouteFailure> {
            let id = field(object_field)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| RouteFailure::Malformed(format!("缺少 {object_field} 字段")))?;
            if self.tasks.iter().any(|task| task.id == id) {
                Ok(id.to_string())
            } else {
                Err(RouteFailure::UnknownTask(id.to_string()))
            }
        };
        match kind.as_str() {
            "new" => {
                let description = field("description")
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| RouteFailure::Malformed("new 缺少 description".into()))?;
                let title = field("title")
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| RouteFailure::Malformed("new 缺少 title".into()))?;
                Ok(Route::New {
                    description: description.to_string(),
                    title: title.chars().take(TITLE_CHARS).collect(),
                })
            }
            "continue" => Ok(Route::Continue {
                task: known_task("task_id")?,
            }),
            "stop" => Ok(Route::Stop {
                task: known_task("task_id")?,
            }),
            "list" if self.scope == RouteScope::Private => Ok(Route::List),
            "ignore" if self.scope == RouteScope::Group => Ok(Route::Ignore),
            "list" | "ignore" => Err(RouteFailure::NotAllowed(kind)),
            other => Err(RouteFailure::Malformed(format!("不认识的 route：{other}"))),
        }
    }

    /// 判断失败时的去向：私聊宁可多开一个任务（任务说明用消息原文，标题由原文截出），群里宁可
    /// 不说话。
    pub fn fallback(&self, reason: RouteFailure, attempts: u32) -> Routed {
        let route = match self.scope {
            RouteScope::Private => Route::New {
                description: self.message.clone(),
                title: title_from(&self.message),
            },
            RouteScope::Group => Route::Ignore,
        };
        Routed {
            route,
            fallback: Some(Fallback { reason, attempts }),
        }
    }
}

/// 判断的去向。`task` 是候选清单里的任务标识（已校验）。
///
/// 范围决定哪些会出现：`List` 只在私聊，`Ignore` 只在群。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Route {
    /// 一件新的事：`description` 是把要求写完整的任务说明。
    New {
        description: String,
        title: String,
    },
    Continue {
        task: String,
    },
    Stop {
        task: String,
    },
    List,
    Ignore,
}

/// 为什么这次判断没成。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouteFailure {
    /// 模型调用报错。
    Model(String),
    /// 答案不合格式。
    Malformed(String),
    /// `continue` / `stop` 指的任务不在候选里。
    UnknownTask(String),
    /// 这个范围没有这个去向（群里的 `list`、私聊里的 `ignore`）。
    NotAllowed(String),
}

impl std::fmt::Display for RouteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteFailure::Model(error) => write!(f, "模型调用失败：{error}"),
            RouteFailure::Malformed(why) => write!(f, "答案不合格式：{why}"),
            RouteFailure::UnknownTask(id) => write!(f, "任务标识不在候选里：{id}"),
            RouteFailure::NotAllowed(route) => write!(f, "这个范围没有 {route} 这个去向"),
        }
    }
}

/// 兜底的记录：原因，以及到放弃时一共调了几次模型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fallback {
    pub reason: RouteFailure,
    pub attempts: u32,
}

/// 一次判断的结果：永远是一个合法去向。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routed {
    pub route: Route,
    /// `Some` = 这个去向是兜底得来的，不是模型的判断。
    pub fallback: Option<Fallback>,
}

/// 模型调用的注入口：只管"给提示、回一段文本"。Gateway 给实现（用 Agent 的 `router_model`）。
#[async_trait]
pub trait RouterModel: Send + Sync {
    /// 错误只是给记录看的一句话；重试与兜底由 [`route`] 决定。
    async fn complete(&self, prompt: &RouterPrompt) -> Result<String, String>;
}

/// 判断：调模型、校验，失败重试到 [`MAX_ATTEMPTS`]，仍失败就兜底。没有副作用，可重跑。
pub async fn route(input: &RouterInput, model: &dyn RouterModel) -> Routed {
    let prompt = input.prompt();
    let mut last = RouteFailure::Model("没有调用".into());
    for attempt in 1..=MAX_ATTEMPTS {
        let failure = match model.complete(&prompt).await {
            Ok(text) => match input.interpret(&text) {
                Ok(route) => {
                    return Routed {
                        route,
                        fallback: None,
                    };
                }
                Err(failure) => failure,
            },
            Err(error) => RouteFailure::Model(error),
        };
        tracing::warn!(%failure, attempt, "Router 这一次没判出去向");
        last = failure;
    }
    input.fallback(last, MAX_ATTEMPTS)
}

/// 标题由消息原文截出（兜底的新任务用它；Gateway 在目标任务已不可用、退成新任务时也用）：第一行非空的内容，最多 [`TITLE_CHARS`] 个字符，截了加 `…`。
pub fn title_from(message: &str) -> String {
    let Some(first) = message.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return "新任务".into();
    };
    let mut title: String = first.chars().take(TITLE_CHARS).collect();
    if first.chars().count() > TITLE_CHARS {
        title.push('…');
    }
    title
}

/// 取回复里第一个能解析成 JSON 对象的 `{…}`：代码围栏与前后的说明文字都不挡路。
fn json_object(text: &str) -> Option<serde_json::Map<String, Value>> {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(offset) = text[from..].find('{') {
        let start = from + offset;
        if let Some(end) = balanced_end(bytes, start)
            && let Ok(Value::Object(object)) = serde_json::from_str(&text[start..end])
        {
            return Some(object);
        }
        from = start + 1;
    }
    None
}

/// `start` 处的 `{` 配对的 `}` 之后的位置；字符串里的括号与转义不算。
fn balanced_end(bytes: &[u8], start: usize) -> Option<usize> {
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (index, &byte) in bytes.iter().enumerate().skip(start) {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests;
