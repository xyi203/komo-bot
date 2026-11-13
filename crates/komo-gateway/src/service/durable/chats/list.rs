//! 对话列表（`GET /v1/chats`）：主对话固定第一，其余在等你的在前、再按最后一句话的时刻倒序。
//!
//! - **最后一句话**取对话当前分支上最近一条用户或 Komo 的文字（用户输入、最终回复、投进来的话）；
//!   触发器起的输入（定时、监听、任务交回）、工具结果、`!` 命令不算。读最近的 [`SCAN_ENTRIES`] 条
//!   entry，找不到就是空串。
//! - **时刻**取会话行的 `last_message_at`，旧会话回落到 `changed_at`、`created_at`。
//! - **在等你**：有指向这段对话的、还没答复的介入（faulted、崩溃停住、待确认），指向关系见
//!   [`DurableCore::route_of`]。`waitingCount` 是这类介入的条数。
//! - **在跑**：这段对话当前有 Run（含停在等你上的）。
//! - 支线对话取活着的 `kind = normal` 会话、排除 `source` 以 `chat:` 开头的存量渠道 / 群会话；
//!   最多给 [`MAX_SIDE_CHATS`] 段（等你的在前，所以不会被截掉）。

use std::collections::HashMap;

use komo_kernel::durable::legacy_events::{MAIN_REPLY_ENTRY, PROJECT_UPDATE_AT};
use komo_kernel::durable::storage::{EntryQuery, SessionQuery};
use komo_kernel::durable::{
    EntryRecord, Message, ScanOrder, SessionCursor, SessionKind, SessionLifecycle, SessionRecord,
};
use komo_kernel::protocol::http::{ChatListResponse, ChatRow};
use komo_kernel::types::ids::SessionId;
use komo_runtime::durable::harness::{Harness, is_trigger_input};

use super::super::DurableCore;
use super::super::sessions::tree::content_text;
use super::super::sessions::{SessionError, time_of};
use super::{Route, is_channel_session};

/// 最后一句话最多留这么多字（按字符，超出补 `…`）。
pub const PREVIEW_CHARS: usize = 80;
/// 往回读多少条 entry 找最后一句话（一个 Run 里工具调用多时，最终回复前面隔着很多 entry）。
const SCAN_ENTRIES: usize = 64;
/// 列表里最多几段支线对话。
const MAX_SIDE_CHATS: usize = 100;
const SCAN_PAGE: usize = 256;

/// 截断成一行预览：空白折成一个空格，超出 [`PREVIEW_CHARS`] 补 `…`。
pub fn preview(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= PREVIEW_CHARS {
        return flat;
    }
    let mut cut: String = flat.chars().take(PREVIEW_CHARS).collect();
    cut.push('…');
    cut
}

/// 一条 entry 里"用户或 Komo 说的话"；不是话（工具、系统、触发器的输入、`!` 命令）是 `None`。
fn words(entry: &EntryRecord) -> Option<String> {
    let kind = entry.kind.as_str();
    if kind == MAIN_REPLY_ENTRY {
        return entry
            .data
            .as_ref()?
            .get("text")?
            .as_str()
            .map(str::to_owned)
            .filter(|text| !text.trim().is_empty());
    }
    if kind == "komo.assistant" {
        return entry
            .model
            .iter()
            .flatten()
            .find_map(|message| match message {
                Message::Assistant(assistant) if !assistant.stop_reason.excluded_from_context() => {
                    let text = assistant
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            komo_kernel::durable::ContentBlock::Text { text, .. } => {
                                Some(text.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    (!text.trim().is_empty()).then_some(text)
                }
                _ => None,
            });
    }
    // 用户的输入：`komo.user` 与各入口自己的 kind（`cli.input` …）。别的 `komo.*`、`!` 命令、触发器起的
    // 输入（定时、监听、任务交回）不是话。
    let plain = kind == "komo.user" || !(kind.starts_with("komo.") || kind == "branch.summary");
    if !plain || kind == "shell.command" || is_trigger_input(kind) || entry.head.is_some() {
        return None;
    }
    let mut text = entry
        .model
        .iter()
        .flatten()
        .find_map(|message| match message {
            Message::User(user) if user.host_origin().is_none() => {
                Some(content_text(&user.content))
            }
            _ => None,
        })?;
    // 输入正文后面可能补了项目规范（只给模型），切回用户说的话。
    if let Some(at) = entry
        .data
        .as_ref()
        .and_then(|data| data.get(PROJECT_UPDATE_AT))
        .and_then(serde_json::Value::as_u64)
        .map(|at| at as usize)
        && text.is_char_boundary(at)
    {
        text.truncate(at);
    }
    (!text.trim().is_empty()).then_some(text)
}

impl DurableCore {
    /// 对话当前分支上最近一句话（见模块文档）。
    pub async fn last_words(&self, record: &SessionRecord) -> Result<String, SessionError> {
        let mut query = EntryQuery::new(record.current_branch);
        query.order = Some(ScanOrder::Descending);
        let page = self
            .storage
            .scan_entries(&query, SCAN_ENTRIES, None)
            .await?;
        Ok(page
            .items
            .iter()
            .find_map(words)
            .map(|text| preview(&text))
            .unwrap_or_default())
    }

    /// 对话列表（见模块文档）。主对话没有就建（同 `/v1/home-session`）。
    pub async fn chats(&self) -> Result<ChatListResponse, SessionError> {
        let main = self.resolve_route(Route::Main).await?;

        // 等你的事：每条介入归到它落话的那段对话。
        let mut waiting: HashMap<String, u32> = HashMap::new();
        for session in self.pending_intervention_sessions().await? {
            let chat = match self.route_of(&session).await? {
                Route::Chat(record) => record.id,
                Route::Main => main.id.clone(),
            };
            *waiting.entry(chat).or_default() += 1;
        }

        let mut sides = self.side_chats().await?;
        sides.sort_by(|a, b| {
            let wa = waiting.contains_key(&a.id);
            let wb = waiting.contains_key(&b.id);
            wb.cmp(&wa)
                .then(b.last_message_ms().cmp(&a.last_message_ms()))
                .then(b.id.cmp(&a.id))
        });
        sides.truncate(MAX_SIDE_CHATS);

        let mut chats = Vec::with_capacity(sides.len() + 1);
        let mut waiting_count = 0;
        for (record, is_main) in
            std::iter::once((&main, true)).chain(sides.iter().map(|r| (r, false)))
        {
            let count = waiting.get(&record.id).copied().unwrap_or(0);
            waiting_count += count;
            let live = Harness::live(&*self.storage, record.current_branch).await?;
            chats.push(ChatRow {
                id: SessionId::from_raw(record.id.clone()),
                main: is_main,
                title: match (&record.title, is_main) {
                    (Some(title), _) if !title.trim().is_empty() => title.clone(),
                    (_, true) => "主对话".to_owned(),
                    (_, false) => "新对话".to_owned(),
                },
                last_message: self.last_words(record).await?,
                last_message_at: time_of(record.last_message_ms()),
                waiting: count > 0,
                running: live.run.is_some(),
            });
        }
        Ok(ChatListResponse {
            chats,
            waiting_count,
        })
    }

    /// 活着的支线对话（`kind = normal`，排除渠道 / 群的存量会话），次序不定。
    async fn side_chats(&self) -> Result<Vec<SessionRecord>, SessionError> {
        let query = SessionQuery {
            lifecycle: Some(SessionLifecycle::Active),
            kind: Some(SessionKind::Normal),
            order: Some(ScanOrder::Descending),
            ..SessionQuery::default()
        };
        let mut found = Vec::new();
        let mut cursor: Option<SessionCursor> = None;
        loop {
            let page = self
                .storage
                .scan_sessions(&query, SCAN_PAGE, cursor.as_ref())
                .await?;
            found.extend(
                page.items
                    .into_iter()
                    .filter(|record| !is_channel_session(record)),
            );
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(found),
            }
        }
    }
}
