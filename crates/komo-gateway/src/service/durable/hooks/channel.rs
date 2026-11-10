//! 渠道篇幅附注（`docs/komo_bot.md` §4「渠道篇幅」，roadmap-muse S5）：[`AgentHooks::context_entries`]
//! 的第二件事，排在记忆召回块之后。
//!
//! 回复要发到聊天渠道的 Run，每个 Run 追加一次一条宿主附上的 user 消息（`komoOrigin = channel`，正文
//! 见 `komo_agent::context::channel`）。"回复发到哪"与看客（`watch.rs`）的投递同一个判断：首个输入是
//! `chat.input` 就是它的 peer；是内部输入（任务结果、监听事件）就按决定 6 找路径上最近的 peer。
//!
//! 每个 Run 判断一次：落一条 [`CHANNEL_NOTE_KIND`] entry（`data.submission` 认 Run），回复要发到渠道
//! 才带 `model`，不发（`api` 对端、内部输入找不到 peer）只是一条记录，让后续轮次不再去查。TUI / HTTP /
//! Cron 起的 Run 首个输入认不出来源，什么都不落——它们本来就不投。任务会话、fork 任务不加：结果交回
//! 派它的会话，由那边转告。
//!
//! [`AgentHooks::context_entries`]: komo_runtime::durable::harness::AgentHooks::context_entries

use std::sync::Arc;

use komo_agent::context::channel::{CHANNEL_ORIGIN, channel_reply_block};
use komo_kernel::durable::{
    Content, EntryRecord, HOST_ORIGIN_KEY, Message, SubmissionId, UserMessage,
};
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform};
use komo_runtime::durable::EntryDraft;
use serde_json::{Value, json};

use super::setup::Resolved;
use crate::service::durable::background::watch::{self, Source};
use crate::service::state::GatewayState;

/// 渠道附注 entry 的 kind（`data {submission, platform?}`）。
pub(crate) const CHANNEL_NOTE_KIND: &str = "komo.channel";

/// 这个 Run 要不要追加附注；已经判断过、或不需要判断时是 `None`。
pub(super) async fn entry(
    state: &Arc<GatewayState>,
    resolved: &Resolved,
    run: Option<SubmissionId>,
    entries: &[EntryRecord],
    now: i64,
) -> Option<EntryDraft> {
    if resolved.is_task() {
        return None;
    }
    let run = run?;
    if noted(entries, run) {
        return None;
    }
    let core = state.core().ok()?;
    let peer = match watch::source_of(core, run).await {
        (Source::Peer(peer), _) => Some(peer),
        (Source::Internal, Some(entry)) => {
            watch::peer_on_path(core, resolved.session.as_str(), entry).await
        }
        _ => return None,
    };
    Some(draft(run, peer.as_ref(), now))
}

/// 附注 entry：回复要发到聊天渠道（不是 `api`）才带 `model`。
pub(super) fn draft(run: SubmissionId, peer: Option<&ChannelPeer>, now: i64) -> EntryDraft {
    let platform = peer
        .map(|peer| &peer.platform)
        .filter(|platform| **platform != ChannelPlatform::Api);
    let mut draft = EntryDraft::new(CHANNEL_NOTE_KIND).with_data(json!({
        "submission": run,
        "platform": platform.map(ChannelPlatform::as_str),
    }));
    if let Some(platform) = platform {
        let mut extra = serde_json::Map::new();
        extra.insert(HOST_ORIGIN_KEY.into(), Value::from(CHANNEL_ORIGIN));
        draft.model = Some(vec![Message::User(UserMessage {
            content: Content::Text(channel_reply_block(platform.as_str())),
            timestamp: now,
            extra,
        })]);
    }
    draft
}

/// 这个 Run 已经落过附注（有没有正文都算）。
fn noted(entries: &[EntryRecord], run: SubmissionId) -> bool {
    entries.iter().any(|entry| {
        entry.kind == CHANNEL_NOTE_KIND
            && entry
                .data
                .as_ref()
                .and_then(|data| data.get("submission"))
                .and_then(|value| serde_json::from_value::<SubmissionId>(value.clone()).ok())
                == Some(run)
    })
}

#[cfg(test)]
mod tests {
    use komo_kernel::durable::{ConversationId, EntryId};

    use super::*;

    fn record(draft: &EntryDraft) -> EntryRecord {
        let mut record = EntryRecord::new(EntryId::new(9), ConversationId::new(1), &draft.kind);
        record.data = draft.data.clone();
        record.model = draft.model.clone();
        record
    }

    /// 发到聊天渠道才带一条宿主附上的 user 消息；`api` 对端与找不到对端只落记录。
    /// 落过的 Run 认得出来，别的 Run 不算。
    #[test]
    fn the_note_carries_a_message_only_for_chat_platforms_and_marks_the_run() {
        let run = SubmissionId::new(7);
        let feishu = ChannelPeer::parse("feishu:oc_1").expect("合法 peer");
        let note = draft(run, Some(&feishu), 5);
        let Some([Message::User(user)]) = note.model.as_deref() else {
            panic!("{note:?}");
        };
        assert_eq!(user.host_origin(), Some(CHANNEL_ORIGIN));
        assert!(
            matches!(&user.content, Content::Text(text) if text.contains("发到飞书")),
            "{user:?}"
        );

        let api = ChannelPeer::parse("api:local").expect("合法 peer");
        assert!(draft(run, Some(&api), 5).model.is_none());
        assert!(draft(run, None, 5).model.is_none());

        let entries = [record(&note)];
        assert!(noted(&entries, run));
        assert!(!noted(&entries, SubmissionId::new(8)));
    }
}
