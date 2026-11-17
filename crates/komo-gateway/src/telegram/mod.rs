//! Telegram 备用入口（`docs/telegram.md`）：网页进不去的时候（隧道断了、登录服务挂了、网页发坏了），只要
//! Gateway 还活着，就能在 Telegram 里跟 Komo 说话、看状态、答复在等你的确认。
//!
//! - **只认一个人**：`[telegram] operator` 那个用户的私聊。别人、群里的消息一律不理，也不回话（不给人试探）。
//! - **收**：长轮询 `getUpdates`（Gateway 主动往外连，不要公开地址）。一句话进主对话，与网页发消息同一条路；
//!   请求键是 `telegram:<update_id>`，重启后同一个 update 再来一次也只受理一次。
//! - **回**：等这条输入结算，把那一轮的文字回复原样发回来（太长按 Telegram 的上限切几段）。
//! - **主动找你**：与浏览器推送同一组时机（[`GatewayState::notify_operator`]）——停住的运行在问你、定时结果、
//!   不是你发起的那一轮说了话。
//! - **命令**：`/status`、`/pending`、`/approve` `/reject` `/resolve` `/abandon`（可带句柄，只有一条能答时省略）。
//!
//! 不做群聊、话题、图片、卡片按钮——2026-11-13 删掉的那套渠道里这些都有，备用入口用不上（`docs/redesign.md` P17）。

pub mod api;

use std::sync::Arc;
use std::time::Duration;

use komo_config::ConfigHolder;
use komo_kernel::protocol::http::{
    InterventionListQuery, InterventionSummary, InterventionVerdict,
};
use komo_kernel::traits::Shutdown;
use komo_kernel::types::chat::PeerId;
use komo_runtime::durable::harness::InputEntry;
use serde_json::json;
use time::format_description::well_known::Rfc3339;

use crate::push::PushMessage;
use crate::service::durable::submit::{DEFAULT_INPUT_KIND, InputRequest};
use crate::service::state::GatewayState;
use api::{BotApi, DEFAULT_ENDPOINT, Message, Update};

/// 一次长轮询挂多久（秒）。
const POLL_SECS: u64 = 25;
/// 轮询失败后的退避：从 2 秒翻倍到 1 分钟。
const BACKOFF_FIRST: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Telegram 一条消息最多 4096 个字符；留点余量给分段标记。
const MESSAGE_CHARS: usize = 4000;

/// 配好了的 bot。没配 `[telegram]`、或 `.env` 里没有 token，Gateway 就没有它。
pub struct Telegram {
    bot: Option<Bot>,
}

struct Bot {
    api: BotApi,
    /// 操作者的用户 id。私聊里 chat id 就是它，发消息也发到这里。
    operator: i64,
}

impl std::fmt::Debug for Telegram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Telegram")
            .field("enabled", &self.bot.is_some())
            .finish()
    }
}

impl Telegram {
    /// 按启动时的配置造。配了却没有 token 记一条警告，当作没配——别让一个漏填的变量拦住 Gateway 启动。
    pub fn from_config(config: &ConfigHolder) -> Self {
        let snapshot = config.current();
        let Some(telegram) = snapshot.start_only.telegram.as_ref() else {
            return Telegram { bot: None };
        };
        let secrets = config.secrets();
        let Some(token) = secrets
            .get(&telegram.token_env)
            .map(str::trim)
            .filter(|token| !token.is_empty())
        else {
            tracing::warn!(
                env = %telegram.token_env,
                "配了 [telegram]，但 .env 里没有 bot token：Telegram 备用入口不开"
            );
            return Telegram { bot: None };
        };
        let endpoint = telegram.endpoint.as_deref().unwrap_or(DEFAULT_ENDPOINT);
        Telegram {
            bot: Some(Bot {
                api: BotApi::new(endpoint, token, reqwest::Client::new()),
                operator: telegram.operator,
            }),
        }
    }

    pub fn enabled(&self) -> bool {
        self.bot.is_some()
    }

    /// 给操作者发一段话。太长切成几段按顺序发；任何失败只记日志——这是备用通道，不能反过来拖住主流程。
    pub async fn send(&self, text: &str) {
        let Some(bot) = &self.bot else { return };
        for part in chunks(text, MESSAGE_CHARS) {
            if let Err(error) = bot.api.send_message(bot.operator, &part).await {
                tracing::warn!(%error, "Telegram 消息没发出去");
                return;
            }
        }
    }

    /// 与浏览器推送同一条消息：标题、正文，配了公开地址再带一个回网页的链接。**立即返回**，后台发。
    pub fn notify(self: &Arc<Self>, message: &PushMessage, public_url: Option<&str>) {
        if self.bot.is_none() {
            return;
        }
        let mut text = format!("{}\n\n{}", message.title, message.body.trim());
        if let Some(base) = public_url.filter(|base| !base.is_empty()) {
            text.push_str(&format!(
                "\n\n{}{}",
                base.trim_end_matches('/'),
                message.url
            ));
        }
        let this = Arc::clone(self);
        tokio::spawn(async move { this.send(&text).await });
    }
}

/// 起长轮询（`background::spawn` 里调）。没开就什么都不做。
pub fn spawn(state: &Arc<GatewayState>, shutdown: &Shutdown) {
    if !state.telegram.enabled() {
        return;
    }
    let state = Arc::clone(state);
    let shutdown = shutdown.clone();
    tokio::spawn(async move { poll(state, shutdown).await });
}

async fn poll(state: Arc<GatewayState>, shutdown: Shutdown) {
    let Some(bot) = &state.telegram.bot else {
        return;
    };
    let mut offset = None;
    let mut backoff = BACKOFF_FIRST;
    while !shutdown.is_cancelled() {
        match bot.api.get_updates(offset, POLL_SECS).await {
            Ok(updates) => {
                backoff = BACKOFF_FIRST;
                for update in updates {
                    // 先推进 offset 再处理：处理里出的错不该让同一个 update 被无限重取。重启后没处理完的那条
                    // 会再来一次，靠请求键去重。
                    offset = Some(update.update_id + 1);
                    handle(&state, bot.operator, update).await;
                }
            }
            Err(error) if error.is_fatal() => {
                tracing::error!(%error, "Telegram bot token 不对或 bot 已被删除，备用入口停了");
                return;
            }
            Err(error) => {
                tracing::warn!(%error, ?backoff, "Telegram 长轮询失败，稍后重试");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/// 一个 update：只收操作者私聊里的文字。
async fn handle(state: &Arc<GatewayState>, operator: i64, update: Update) {
    let Some(message) = update.message else {
        return;
    };
    if !from_operator(&message, operator) {
        tracing::debug!(chat = message.chat.id, "Telegram：不是操作者的私聊，不理");
        return;
    }
    let Some(text) = message
        .text
        .as_deref()
        .or(message.caption.as_deref())
        .map(str::trim)
        .filter(|text| !text.is_empty())
    else {
        state
            .telegram
            .send("备用入口只收文字。图片和文件请回网页发。")
            .await;
        return;
    };
    let reply = match Command::parse(text) {
        Some(command) => command.run(state, operator).await,
        None => {
            submit(state, update.update_id, text).await;
            return;
        }
    };
    state.telegram.send(&reply).await;
}

fn from_operator(message: &Message, operator: i64) -> bool {
    message.chat.kind == "private"
        && message
            .from
            .as_ref()
            .is_some_and(|from| from.id == operator)
}

/// 一句话进主对话；结算后把回复发回来（后台等，不挡下一条 update）。
async fn submit(state: &Arc<GatewayState>, update_id: i64, text: &str) {
    let accepted = async {
        let core = state.core().map_err(|error| error.to_string())?;
        let agent = state.snapshot().agent.default_agent.clone();
        let (record, _) = core
            .main_session(&agent)
            .await
            .map_err(|error| error.to_string())?;
        let mut request = InputRequest::new(text);
        request.request_id = Some(format!("telegram:{update_id}"));
        request.entry = Some(InputEntry::new(
            DEFAULT_INPUT_KIND,
            Some(json!({ "via": "telegram" })),
        ));
        core.submit_input(&record.id, request)
            .await
            .map_err(|error| error.to_string())
    }
    .await;
    let accepted = match accepted {
        Ok(accepted) => accepted,
        Err(error) => {
            tracing::warn!(%error, "Telegram 的消息没进主对话");
            state.telegram.send(&format!("没收进去：{error}")).await;
            return;
        }
    };
    // 同一个 update 重启后再来一次：原来那条已经在跑或跑完了，它的回复由第一次的等待负责。
    if accepted.deduplicated {
        return;
    }
    let state = Arc::clone(state);
    tokio::spawn(async move {
        let reply = match state.core() {
            Ok(core) => match core.wait_submission(accepted.submission).await {
                Ok(record) => {
                    crate::service::durable::background::watch::completed_text(&core, &record)
                        .await
                        .unwrap_or_else(|| {
                            "这一轮没有完成（被停下或出错了），回网页看看过程。".to_owned()
                        })
                }
                Err(error) => format!("等回复时出错了：{error}"),
            },
            Err(error) => format!("等回复时出错了：{error}"),
        };
        state.telegram.send(&reply).await;
    });
}

// ------------------------------------------------------------------ 命令

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Help,
    Status,
    Pending,
    Answer {
        verdict: InterventionVerdict,
        handle: Option<String>,
    },
}

impl Command {
    /// `/命令[@bot名] [参数]`。不以 `/` 开头的不是命令，交给主对话。认不出的命令当 `/help`。
    fn parse(text: &str) -> Option<Command> {
        let rest = text.strip_prefix('/')?;
        let mut words = rest.split_whitespace();
        let name = words.next().unwrap_or_default();
        // 群里或带了 bot 名的写法：`/status@komo_bot`。
        let name = name
            .split('@')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let arg = words.next().map(str::to_owned);
        Some(match name.as_str() {
            "status" => Command::Status,
            "pending" => Command::Pending,
            other => match InterventionVerdict::parse(other) {
                Some(verdict) => Command::Answer {
                    verdict,
                    handle: arg,
                },
                None => Command::Help,
            },
        })
    }

    async fn run(self, state: &Arc<GatewayState>, operator: i64) -> String {
        match self {
            Command::Help => HELP.to_owned(),
            Command::Status => status(state).await,
            Command::Pending => match pending(state).await {
                Ok(list) if list.is_empty() => "没有在等你答复的。".to_owned(),
                Ok(list) => list.iter().map(describe).collect::<Vec<_>>().join("\n\n"),
                Err(error) => format!("读不出待处理清单：{error}"),
            },
            Command::Answer { verdict, handle } => {
                answer(state, operator, verdict, handle.as_deref()).await
            }
        }
    }
}

const HELP: &str = "直接发文字就是在主对话里说话。\n\n\
/status 看 Gateway 在不在、有几条在等你\n\
/pending 列出在等你答复的\n\
/approve /reject 答复确认卡（在等的只有一条时可以不带句柄）\n\
/resolve /abandon 答复卡住的运行";

async fn status(state: &Arc<GatewayState>) -> String {
    let started = state
        .started_at
        .format(&Rfc3339)
        .unwrap_or_else(|_| state.started_at.to_string());
    let waiting = match pending(state).await {
        Ok(list) => format!("{} 条", list.len()),
        Err(error) => format!("读不出（{error}）"),
    };
    format!(
        "Komo Gateway 在跑\n版本 {}\n启动于 {started}\n在等你答复：{waiting}",
        env!("CARGO_PKG_VERSION")
    )
}

async fn pending(state: &Arc<GatewayState>) -> Result<Vec<InterventionSummary>, String> {
    state
        .interventions(&InterventionListQuery::default())
        .await
        .map_err(|error| error.to_string())
}

fn describe(item: &InterventionSummary) -> String {
    let verdicts = item
        .verdicts
        .iter()
        .map(|verdict| format!("/{} {}", verdict.as_str(), item.handle))
        .collect::<Vec<_>>()
        .join("  ");
    format!(
        "【{}】{}\n{}",
        item.kind.as_str(),
        item.question.trim(),
        verdicts
    )
}

/// 答复。没给句柄时：在等的里面能答这个结论的恰好一条就答它，否则列出来让人挑。
async fn answer(
    state: &Arc<GatewayState>,
    operator: i64,
    verdict: InterventionVerdict,
    handle: Option<&str>,
) -> String {
    let handle = match handle {
        Some(handle) => handle.to_owned(),
        None => {
            let list = match pending(state).await {
                Ok(list) => list,
                Err(error) => return format!("读不出待处理清单：{error}"),
            };
            let candidates: Vec<_> = list
                .into_iter()
                .filter(|item| item.verdicts.contains(&verdict))
                .collect();
            match candidates.as_slice() {
                [] => return format!("没有能 /{} 的。", verdict.as_str()),
                [only] => only.handle.clone(),
                many => {
                    return format!(
                        "有 {} 条能 /{}，带上句柄：\n\n{}",
                        many.len(),
                        verdict.as_str(),
                        many.iter().map(describe).collect::<Vec<_>>().join("\n\n")
                    );
                }
            }
        }
    };
    let by = PeerId::new(format!("telegram:{operator}"));
    match state.answer_intervention(&handle, verdict, Some(by)).await {
        Ok(answered) => answered.note,
        Err(error) => format!("没答上：{error}"),
    }
}

/// 按字符数切段，尽量在换行处断开。
fn chunks(text: &str, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        if rest.chars().count() <= limit {
            out.push(rest.to_owned());
            break;
        }
        let cut = rest
            .char_indices()
            .nth(limit)
            .map_or(rest.len(), |(index, _)| index);
        let head = &rest[..cut];
        let at = head.rfind('\n').filter(|at| *at > cut / 2).unwrap_or(cut);
        out.push(rest[..at].trim_end().to_owned());
        rest = rest[at..].trim_start();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_not_a_command() {
        assert_eq!(Command::parse("帮我看下发布"), None);
        assert_eq!(Command::parse("/status"), Some(Command::Status));
        assert_eq!(Command::parse("/status@komo_bot"), Some(Command::Status));
        assert_eq!(Command::parse("/PENDING"), Some(Command::Pending));
        assert_eq!(Command::parse("/start"), Some(Command::Help));
    }

    #[test]
    fn a_verdict_command_takes_an_optional_handle() {
        assert_eq!(
            Command::parse("/approve 42"),
            Some(Command::Answer {
                verdict: InterventionVerdict::Approve,
                handle: Some("42".into()),
            })
        );
        assert_eq!(
            Command::parse("/reject"),
            Some(Command::Answer {
                verdict: InterventionVerdict::Reject,
                handle: None,
            })
        );
    }

    #[test]
    fn long_text_is_cut_at_line_breaks_within_the_limit() {
        let text = format!("{}\n{}", "a".repeat(8), "b".repeat(8));
        assert_eq!(chunks(&text, 12), vec!["a".repeat(8), "b".repeat(8)]);
        let one_line = "字".repeat(25);
        let parts = chunks(&one_line, 10);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|part| part.chars().count() <= 10));
        assert_eq!(parts.concat(), one_line);
        assert!(chunks("  ", 10).is_empty());
    }
}
