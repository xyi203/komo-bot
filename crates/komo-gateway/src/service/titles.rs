//! 首轮跑完后给会话起个标题。
//!
//! `accept_input` 先把第一条输入的首行写成标题（[`komo_store::coordinator::title_for`]），
//! 那是个临时值：长消息截成 60 字、"帮我看看这个"一类的话认不出是哪个会话。第一条 Run
//! 正常完成后，这里拿"用户说的 + 最终回复"问一次记忆模型（不给工具），把临时值换掉。
//!
//! - 只管普通会话：主会话是长期入口，一轮对话代表不了它；任务会话的标题派出时就写死了。
//! - 只在标题**还是临时值**时换（[`session::replace_title_if`]）——别处写过的不碰。
//! - 失败只记日志：标题是给人看的，没换成就留着临时值。

use std::sync::Arc;

use komo_kernel::events::EventPayload;
use komo_kernel::traits::LlmClient as _;
use komo_kernel::types::ids::{RunId, SessionId};
use komo_kernel::types::turn::{ReplayMessage, Role, RoundInput, TurnRequest};
use komo_store::models::SessionKind;
use komo_store::repos::session;

use super::state::GatewayState;

/// 生成的标题最多这么多个字符。
const TITLE_MAX_CHARS: usize = 24;
/// 喂给模型的两段各截到这么长：起标题用不着全文。
const EXCERPT_CHARS: usize = 1500;

const SYSTEM_PROMPT: &str = "给下面这段对话起一个简短的标题，让人在会话列表里一眼认出它在做什么。\n\
要求：中文为主，不超过 20 个字；概括用户要做的事，不要照抄原话；\n\
只输出标题本身，不加引号、书名号、句号，也不要\"标题：\"这类前缀。";

/// Run 正常完成之后调：不等它，后台去起标题。
pub fn spawn(state: &Arc<GatewayState>, session: &SessionId, run: &RunId, reply: &str) {
    if !state.auto_title {
        return;
    }
    let (state, session, run, reply) = (
        Arc::clone(state),
        session.clone(),
        run.clone(),
        reply.to_string(),
    );
    tokio::spawn(async move {
        if let Err(error) = retitle(&state, &session, &run, &reply).await {
            tracing::debug!(%error, %session, "会话标题没换成，留着首行");
        }
    });
}

async fn retitle(
    state: &GatewayState,
    session: &SessionId,
    run: &RunId,
    reply: &str,
) -> Result<(), String> {
    let record = session::get(&state.db, session)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("会话不在了")?;
    if record.kind != SessionKind::Normal {
        return Ok(());
    }
    let events = super::compaction::read_events(state.routed.as_ref(), session)
        .await
        .map_err(|error| error.to_string())?;
    // `!` 直接执行的命令全程不发模型请求，起标题也不例外。
    if is_direct_command(&events, run) {
        return Ok(());
    }
    let Some(input) = first_turn(&events, run) else {
        return Ok(());
    };
    // 标题已经不是首行（任务会话、手改过、上一次已经换过）就不动。
    if komo_store::coordinator::title_for(&input).as_deref() != Some(record.title.as_str()) {
        return Ok(());
    }

    let model = state.snapshot().memory.model.clone();
    let request = TurnRequest {
        session: session.clone(),
        run: run.clone(),
        model: model.clone(),
        system_prompt: SYSTEM_PROMPT.to_string(),
        messages: vec![ReplayMessage {
            role: Role::User,
            seq: komo_kernel::types::ids::Seq::ZERO,
            text: Some(format!(
                "用户：{}\n\n助手：{}",
                excerpt(&input),
                excerpt(reply)
            )),
            tool_calls: vec![],
            tool_results: vec![],
            provider_blocks: None,
        }],
        tools: vec![],
        memories: vec![],
        covers: None,
        cache_session: None,
    };
    let mut driver = state
        .llm
        .begin_turn(request)
        .await
        .map_err(|error| error.to_string())?;
    let round = komo_runtime::telemetry::turn(
        &mut *driver,
        RoundInput::First,
        komo_runtime::telemetry::role::TITLE,
        &model,
        1,
    )
    .await
    .map_err(|error| error.to_string())?;
    let Some(title) = clean(round.text.as_deref().unwrap_or_default()) else {
        return Err("模型没给出能用的标题".into());
    };
    session::replace_title_if(&state.db, session, &record.title, &title)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// 这条 Run 是不是 TUI 的 `!`（快照里冻结了 `direct_command`）。
fn is_direct_command(events: &[komo_kernel::events::Event], run: &RunId) -> bool {
    events.iter().any(|event| match &event.payload {
        EventPayload::RunAccepted(body) if event.run.as_ref() == Some(run) => body
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.direct_command.is_some()),
        _ => false,
    })
}

/// 这条 Run 是不是会话里**第一条**完成的；是就交回第一条用户输入。
fn first_turn(events: &[komo_kernel::events::Event], run: &RunId) -> Option<String> {
    let first_completed = events
        .iter()
        .find(|event| matches!(event.payload, EventPayload::RunCompleted(_)))?;
    if first_completed.run.as_ref() != Some(run) {
        return None;
    }
    events.iter().find_map(|event| match &event.payload {
        EventPayload::MessageUser(body) => body.text.clone().filter(|text| !text.trim().is_empty()),
        _ => None,
    })
}

fn excerpt(text: &str) -> String {
    let mut out: String = text.trim().chars().take(EXCERPT_CHARS).collect();
    if text.trim().chars().count() > EXCERPT_CHARS {
        out.push('…');
    }
    out
}

/// 从模型正文里取标题：第一行非空、去掉前缀与包裹的标点，截到 [`TITLE_MAX_CHARS`]。
fn clean(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let line = ["标题：", "标题:", "Title:", "title:"]
        .iter()
        .find_map(|prefix| line.strip_prefix(prefix))
        .unwrap_or(line)
        .trim();
    // 句号可能在书名号外面（`《…》。`），所以收尾标点与包裹的符号一起剥。
    let line = line
        .trim_start_matches(|c: char| wrapping(c) || c.is_whitespace())
        .trim_end_matches(|c: char| {
            wrapping(c)
                || c.is_whitespace()
                || matches!(c, '。' | '.' | '！' | '!' | '；' | ';' | '，' | ',')
        });
    if line.is_empty() {
        return None;
    }
    Some(line.chars().take(TITLE_MAX_CHARS).collect())
}

fn wrapping(c: char) -> bool {
    matches!(
        c,
        '"' | '\'' | '“' | '”' | '‘' | '’' | '《' | '》' | '「' | '」' | '*' | '#' | '`'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::events::{EVENT_FORMAT_VERSION, Event, MessageUser, RunCompleted};
    use komo_kernel::types::ids::{EventId, Seq};

    fn event(seq: u64, run: &str, payload: EventPayload) -> Event {
        Event {
            v: EVENT_FORMAT_VERSION,
            seq: Seq(seq),
            event_id: EventId::from_raw(format!("e-{seq}")),
            session: SessionId::from_raw("sess-1"),
            run: Some(RunId::from_raw(run)),
            ts: time::OffsetDateTime::UNIX_EPOCH,
            payload,
        }
    }

    fn user(seq: u64, run: &str, text: &str) -> Event {
        event(
            seq,
            run,
            EventPayload::MessageUser(MessageUser {
                text: Some(text.into()),
                text_ref: None,
            }),
        )
    }

    fn completed(seq: u64, run: &str) -> Event {
        event(
            seq,
            run,
            EventPayload::RunCompleted(RunCompleted {
                final_message: Some("好了".into()),
                final_message_ref: None,
                rounds: 1,
            }),
        )
    }

    #[test]
    fn only_the_first_completed_run_gets_a_title() {
        let events = vec![
            user(1, "run-1", "帮我看看这个报错"),
            completed(2, "run-1"),
            user(3, "run-2", "再来一个"),
            completed(4, "run-2"),
        ];
        assert_eq!(
            first_turn(&events, &RunId::from_raw("run-1")).as_deref(),
            Some("帮我看看这个报错")
        );
        assert_eq!(first_turn(&events, &RunId::from_raw("run-2")), None);
    }

    #[test]
    fn the_title_is_stripped_of_wrapping_and_prefixes() {
        assert_eq!(
            clean("标题：《排查 shell 报错》。\n多余的话").as_deref(),
            Some("排查 shell 报错")
        );
        assert_eq!(
            clean("\n\n  \"关闭客厅空调\"  ").as_deref(),
            Some("关闭客厅空调")
        );
        assert_eq!(clean("**komo 优化建议**").as_deref(), Some("komo 优化建议"));
        assert_eq!(clean("   \n "), None);
        assert_eq!(clean("「」"), None);
        let long = "一".repeat(40);
        assert_eq!(clean(&long).unwrap().chars().count(), TITLE_MAX_CHARS);
    }
}
