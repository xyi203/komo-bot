//! 跨会话找回（`komo session search`，roadmap-muse S3；`docs/komo_bot.md` §8.11「跨会话找回」）。
//!
//! 飞书一个话题一个 Session、私聊进 home，彼此看不到；记忆提取不保证带上链接、ID 这类细节。这里给一个
//! 只读的入口：在会话标题与 entry 正文（用户的话、助手的回答、工具结果）里按关键词找，返回命中的会话、
//! 片段与能接着查的 ID（会话 ID、任务短号、还在跑的监听）。
//!
//! - **匹配**：空白分开的几个词，同一条消息里都出现才算（不分大小写）；标题命中也算一条。宿主附上的
//!   消息（召回块、渠道附注）与 system 消息不看。
//! - **范围**：`active` / `closing` 的会话；会话的 Agent 是 `memory = private` / `off` 的不出现
//!   （同 `komo memory search` 的可见性：别人的话不进操作者的检索）。任务会话没记 Agent 时按父会话的。
//! - **次序与上限**：按最近一条命中的时刻倒序，默认 10 个会话、最多 50；每个会话留最近 3 个片段。
//! - 每个 conversation 只看它自己的 entry（`scan_entries` 会带上 fork 祖先，那些算在祖先所在的会话里）。

use std::collections::HashMap;

use komo_kernel::durable::storage::EntryQuery;
use komo_kernel::durable::{
    ContentBlock, Cursor, EntryRecord, Message, ScanOrder, SessionKind, SessionRecord,
};
use komo_kernel::protocol::http::SessionSearchHit;
use komo_kernel::types::agent::{AgentConfig, MemoryMode};
use komo_kernel::types::ids::SessionId;
use komo_runtime::durable::harness::orchestration::monitor;

use super::super::DurableCore;
use super::runs::session_conversations;
use super::tasks::handle;
use super::tree::content_text;
use super::{SessionError, time_of};

/// 没给 `limit` 时给几个会话。
pub const DEFAULT_LIMIT: usize = 10;
/// `limit` 的上限。
pub const MAX_LIMIT: usize = 50;
/// 每个会话留几个片段。
const SNIPPETS: usize = 3;
/// 片段在命中处之前、之后各留多少字。
const BEFORE: usize = 30;
const AFTER: usize = 90;
const SCAN_PAGE: usize = 256;

impl DurableCore {
    /// 在会话标题与正文里找 `query`（见模块文档）。`agents` 是当前配置里的 Agent，用来判可见性。
    pub async fn search_sessions(
        &self,
        query: &str,
        limit: Option<usize>,
        agents: &AgentConfig,
    ) -> Result<Vec<SessionSearchHit>, SessionError> {
        let terms = terms(query);
        if terms.is_empty() {
            return Err(SessionError::Invalid("要给关键词".into()));
        }
        let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let records = self.list_sessions(false).await?;
        let by_id: HashMap<&str, &SessionRecord> =
            records.iter().map(|r| (r.id.as_str(), r)).collect();
        let monitors = monitor::list(&*self.storage).await?;

        let mut hits = Vec::new();
        for record in &records {
            if !visible(record, &by_id, agents) {
                continue;
            }
            let mut found: Vec<(i64, String)> = Vec::new();
            if let Some(title) = &record.title
                && matches_all(title, &terms)
            {
                found.push((record.changed_at, format!("标题：{title}")));
            }
            let conversations = session_conversations(&*self.storage, &record.id).await?;
            for conversation in &conversations {
                for entry in own_entries(self, conversation.id).await? {
                    for (at, role, text) in texts(&entry) {
                        if let Some(snippet) = snippet(&text, &terms) {
                            found.push((at, format!("{role}：{snippet}")));
                        }
                    }
                }
            }
            if found.is_empty() {
                continue;
            }
            found.sort_by(|a, b| b.0.cmp(&a.0));
            let ids: Vec<_> = conversations.iter().map(|c| c.id).collect();
            hits.push(SessionSearchHit {
                session: SessionId::from_raw(record.id.clone()),
                title: record.title.clone().unwrap_or_default(),
                kind: kind_word(record.kind).to_owned(),
                matched_at: time_of(found[0].0),
                task: (record.kind == SessionKind::Task).then(|| handle(record.root_conversation)),
                parent_session: record.parent_session.clone().map(SessionId::from_raw),
                monitors: monitors
                    .iter()
                    .filter(|m| m.is_live() && ids.contains(&m.conversation))
                    .map(|m| format!("#{}「{}」", m.handle(), m.title))
                    .collect(),
                matches: found.len() as u32,
                snippets: found.into_iter().take(SNIPPETS).map(|(_, s)| s).collect(),
            });
        }
        hits.sort_by(|a, b| b.matched_at.cmp(&a.matched_at));
        hits.truncate(limit);
        Ok(hits)
    }
}

/// 关键词：空白分开、转小写。
fn terms(query: &str) -> Vec<Vec<char>> {
    query
        .split_whitespace()
        .map(|term| lower(term))
        .filter(|term| !term.is_empty())
        .collect()
}

/// 一个字一个字转小写（长度不变，位置对得上原文）。
fn lower(text: &str) -> Vec<char> {
    text.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

fn find(haystack: &[char], needle: &[char]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| haystack[i..i + needle.len()] == *needle)
}

fn matches_all(text: &str, terms: &[Vec<char>]) -> bool {
    let low = lower(text);
    terms.iter().all(|term| find(&low, term).is_some())
}

/// 全部词都出现时，第一个词所在处前后的一段（换行压成空格，截了就带「…」）。
fn snippet(text: &str, terms: &[Vec<char>]) -> Option<String> {
    let low = lower(text);
    let mut at = None;
    for term in terms {
        let pos = find(&low, term)?;
        at.get_or_insert(pos);
    }
    let at = at?;
    let chars: Vec<char> = text.chars().collect();
    let start = at.saturating_sub(BEFORE);
    let end = (at + AFTER).min(chars.len());
    let body: String = chars[start..end]
        .iter()
        .map(|c| if c.is_whitespace() { ' ' } else { *c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    Some(format!(
        "{}{body}{}",
        if start > 0 { "…" } else { "" },
        if end < chars.len() { "…" } else { "" }
    ))
}

/// 会话的 Agent 记忆模式不是 private / off。任务会话没记 Agent 就看父会话；都没有按默认 Agent。
fn visible(
    record: &SessionRecord,
    by_id: &HashMap<&str, &SessionRecord>,
    agents: &AgentConfig,
) -> bool {
    let mut current = record;
    for _ in 0..8 {
        if let Some(agent) = current.agent.as_deref().filter(|a| !a.is_empty()) {
            let profile = agents
                .get(agent)
                .unwrap_or_else(|| agents.default_profile());
            return profile.memory == MemoryMode::Shared;
        }
        match current.parent_session.as_deref().and_then(|p| by_id.get(p)) {
            Some(parent) => current = parent,
            None => break,
        }
    }
    agents.default_profile().memory == MemoryMode::Shared
}

/// 这个 conversation 自己的 entry（不含 fork 祖先的），升序。
async fn own_entries(
    core: &DurableCore,
    conversation: komo_kernel::durable::ConversationId,
) -> Result<Vec<EntryRecord>, SessionError> {
    let query = EntryQuery {
        order: Some(ScanOrder::Ascending),
        ..EntryQuery::new(conversation)
    };
    let mut out = Vec::new();
    let mut cursor: Option<Cursor> = None;
    loop {
        let page = core
            .storage
            .scan_entries(&query, SCAN_PAGE, cursor.as_ref())
            .await?;
        out.extend(
            page.items
                .into_iter()
                .filter(|entry| entry.conversation == conversation),
        );
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(out),
        }
    }
}

/// 一条 entry 里能搜的文字：`(时刻, 谁, 正文)`。宿主附上的消息与 system 消息不算。
fn texts(entry: &EntryRecord) -> Vec<(i64, String, String)> {
    entry
        .model
        .iter()
        .flatten()
        .filter_map(|message| match message {
            Message::User(user) if user.host_origin().is_none() => Some((
                user.timestamp,
                "用户".to_owned(),
                content_text(&user.content),
            )),
            Message::Assistant(assistant) => {
                let text = blocks_text(&assistant.content);
                Some((assistant.timestamp, "助手".to_owned(), text))
            }
            Message::ToolResult(result) => Some((
                result.timestamp,
                format!("工具 {}", result.tool_name),
                blocks_text(&result.content),
            )),
            _ => None,
        })
        .filter(|(_, _, text)| !text.trim().is_empty())
        .collect()
}

fn blocks_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn kind_word(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Main => "main",
        SessionKind::Normal => "normal",
        SessionKind::Task => "task",
        SessionKind::Cron => "cron",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::durable::sessions::NewSession;
    use crate::service::durable::submit::InputRequest;
    use crate::service::test_support::harness::{FakeLlm, GatewayBuilder, config_toml};

    /// 走 `GET /v1/session-search`：命中别的会话里的链接，给出会话 ID 与片段；`memory = private` 的
    /// Agent 的会话不出现；空关键词 400。
    #[tokio::test]
    async fn search_finds_a_link_in_another_session_but_not_in_a_private_agents() {
        let gateway = GatewayBuilder::new(&config_toml("[agents.helper]\nmemory = \"private\"\n"))
            .fake_llm(FakeLlm::finisher("收到"))
            .start()
            .await;
        let core = gateway.state().core().unwrap().clone();
        let shared = core
            .create_session(NewSession::new("assistant"))
            .await
            .unwrap();
        let private = core
            .create_session(NewSession::new("helper"))
            .await
            .unwrap();
        for (session, key) in [(&shared, "s:1"), (&private, "p:1")] {
            let accepted = core
                .submit_input(
                    &session.id,
                    InputRequest::new("看看 DeepSeek 那篇文章 https://mp.weixin.qq.com/s/zoS7")
                        .request_id(key),
                )
                .await
                .unwrap();
            gateway.wait_run(&accepted.run).await;
        }

        let (status, body) = gateway
            .get_json("/v1/session-search?q=deepseek%20weixin")
            .await;
        assert_eq!(status, 200, "{body}");
        let hits = body["hits"].as_array().expect("数组");
        assert_eq!(hits.len(), 1, "{body}");
        assert_eq!(hits[0]["session"], shared.id.as_str());
        // 临时标题取自第一条消息，标题也算一处。
        let snippets: Vec<&str> = hits[0]["snippets"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s.as_str())
            .collect();
        assert!(
            snippets
                .iter()
                .any(|s| s.starts_with("用户：") && s.contains("mp.weixin.qq.com/s/zoS7")),
            "{snippets:?}"
        );

        let (status, body) = gateway.get_json("/v1/session-search?q=%20").await;
        assert_eq!(status, 400, "{body}");
    }

    /// 几个词都在才算；不分大小写；片段围着第一个词，截了带「…」，换行压成空格。
    #[test]
    fn a_snippet_needs_every_term_and_centres_on_the_first() {
        let text = format!(
            "{}看看 DeepSeek 那篇\n文章：https://mp.weixin.qq.com/s/zoS7 {}",
            "前".repeat(40),
            "后".repeat(120)
        );
        let wanted = terms("deepseek 文章");
        let got = snippet(&text, &wanted).expect("命中");
        assert!(got.starts_with('…') && got.ends_with('…'), "{got}");
        assert!(got.contains("DeepSeek 那篇 文章"), "{got}");
        assert!(snippet(&text, &terms("deepseek 没有的词")).is_none());
        assert!(terms("   ").is_empty());
    }
}
