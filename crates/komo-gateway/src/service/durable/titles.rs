//! 首轮跑完后给会话起个标题（`docs/storage-durable-d5.md` §8 的"标题"一行）。标题读写 `sessions`
//! 行的 `title` / `title_provisional`：
//!
//! - 受理第一条输入时 `land` 把首行写成临时标题（`title_provisional = true`）；
//! - 第一条 run 正常完成后，这里拿"用户说的 + 最终回复"问一次记忆模型（不给工具），把临时值换掉；
//! - 只管普通会话（`kind = normal`）；只在标题**还是这条输入的首行**时换，别处写过的不碰；
//! - 失败只记日志：标题是给人看的，没换成就留着临时值。

use std::sync::Arc;

use komo_kernel::durable::{SessionKind, legacy_ids};
use komo_kernel::traits::LlmClient as _;
use komo_kernel::types::ids::{RunId, SessionId};
use komo_kernel::types::turn::{ReplayMessage, Role, RoundInput, TurnRequest};

use super::tree::input_text;
use crate::service::state::GatewayState;

/// 生成的标题最多这么多个字符。
const TITLE_MAX_CHARS: usize = 24;
/// 喂给模型的两段各截到这么长：起标题用不着全文。
const EXCERPT_CHARS: usize = 1500;

const SYSTEM_PROMPT: &str = "给下面这段对话起一个简短的标题，让人在会话列表里一眼认出它在做什么。\n\
要求：中文为主，不超过 20 个字；概括用户要做的事，不要照抄原话；\n\
只输出标题本身，不加引号、书名号、句号，也不要\"标题：\"这类前缀。";

/// Run 正常完成之后调：不等它，后台去起标题。
pub fn spawn(state: &Arc<GatewayState>, session: &str, run: &RunId, reply: &str) {
    if !state.auto_title {
        return;
    }
    let (state, session, run, reply) = (
        Arc::clone(state),
        session.to_owned(),
        run.clone(),
        reply.to_owned(),
    );
    tokio::spawn(async move {
        if let Err(error) = retitle(&state, &session, &run, &reply).await {
            tracing::debug!(%error, %session, "会话标题没换成，留着首行");
        }
    });
}

async fn retitle(
    state: &GatewayState,
    session: &str,
    run: &RunId,
    reply: &str,
) -> Result<(), String> {
    let core = state.core().map_err(|error| error.to_string())?;
    let record = core.session(session).await.map_err(|e| e.to_string())?;
    if record.kind != SessionKind::Normal || !record.title_provisional {
        return Ok(());
    }
    let first = legacy_ids::submission_of(run).ok_or("不是 Durable 的 RunId")?;
    let (_, entry) = super::watch::source_of(core, first).await;
    let entry = entry.ok_or("输入没有放置过")?;
    let stored = core
        .storage
        .entry(entry)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("输入 entry 不在了")?;
    let Some(input) = input_text(&stored.entry).filter(|text| !text.trim().is_empty()) else {
        return Ok(());
    };
    // 标题已经不是这条输入的首行（手改过、上一次已经换过）就不动。
    let Some(current) = record.title.clone() else {
        return Ok(());
    };
    if title_for(&input).as_deref() != Some(current.as_str()) {
        return Ok(());
    }

    let model = state.snapshot().memory.model.clone();
    let request = TurnRequest {
        session: SessionId::from_raw(session),
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
    replace_title_if(core, session, &current, &title).await
}

/// 标题还是 `expected`（且仍是临时值）才换：在一次提交里读改写。
async fn replace_title_if(
    core: &super::DurableCore,
    session: &str,
    expected: &str,
    title: &str,
) -> Result<(), String> {
    let (id, expected, title) = (session.to_owned(), expected.to_owned(), title.to_owned());
    core.engine
        .commit(move |tx| {
            Box::pin(async move {
                if let Some(mut record) = tx.session(&id).await?
                    && record.title_provisional
                    && record.title.as_deref() == Some(expected.as_str())
                {
                    record.title = Some(title);
                    record.title_provisional = false;
                    tx.put_session(record);
                }
                Ok(())
            })
        })
        .await
        .map_err(|error| error.to_string())
}

/// 会话标题取哪一段：输入的**第一行**（去掉首尾空白），最多 [`PROVISIONAL_TITLE_CHARS`] 个字符。
///
/// 空白输入没有标题可言（返回 `None`，于是空会话在列表里仍然是"无标题"，而不是一行空白）。
/// 截断按**字符**切，不按字节——中文按字节切会切出半个字。
pub(crate) fn title_for(text: &str) -> Option<String> {
    let first = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let mut title: String = first.chars().take(PROVISIONAL_TITLE_CHARS).collect();
    if first.chars().count() > PROVISIONAL_TITLE_CHARS {
        title.push('…');
    }
    Some(title)
}

/// 标题最多这么多个字符。够列表里认出是哪个会话，又不至于把整段话塞进一列。
const PROVISIONAL_TITLE_CHARS: usize = 60;

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

    #[test]
    fn the_provisional_title_is_the_first_line_cut_by_characters() {
        assert_eq!(title_for("  \n 你好\n第二行").as_deref(), Some("你好"));
        assert_eq!(title_for("  \n "), None);
        let long = "字".repeat(61);
        let title = title_for(&long).unwrap();
        assert_eq!(title.chars().count(), 61, "60 个字符加省略号");
        assert!(title.ends_with('…'));
    }

    #[test]
    fn the_title_is_stripped_of_wrapping_and_prefixes() {
        assert_eq!(
            clean("标题：《排查 shell 报错》。\n多余的话").as_deref(),
            Some("排查 shell 报错")
        );
        assert_eq!(clean("   \n "), None);
        assert_eq!(clean("「」"), None);
        let long = "一".repeat(40);
        assert_eq!(clean(&long).unwrap().chars().count(), TITLE_MAX_CHARS);
    }
}
