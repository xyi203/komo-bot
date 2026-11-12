//! 对话位置的路由归属（`docs/tasks.md` §4.7、§4.14 F5）：一个带话题的 `ChannelPeer` 至多一个任务拥有它，
//! 归属记在任务的 `komo.task.thread`（派出那次提交里写定），不另建表。
//!
//! 查归属 = 在这条线（主会话 / 群的线）派出的、还活着的任务会话里找 `thread` 等于这个位置的那个。
//! 只看这条线派出的任务：群的线上进来的消息不可能直通到操作者私聊里的任务，反过来也一样。
//! 范围与 Router 的候选清单同一个口径（[`super::input`]），不单独扫全部会话。
//!
//! 直通的判断只在 [`super::judge`] 一处：分发任务排到轮次时（同一条线上更早的消息已分发完）才查，
//! 所以刚被前一条消息开出来的归属也看得见。

use komo_kernel::durable::SessionKind;
use komo_kernel::durable::SessionLifecycle;
use komo_kernel::durable::storage::{SessionCursor, SessionQuery};
use komo_kernel::durable::{ConversationId, ScanOrder};
use komo_kernel::types::chat::ChannelPeer;
use komo_runtime::durable::harness::task_doc;

use super::super::DurableCore;
use super::super::sessions::SessionError;

/// 位置上现在的归属任务。
pub(super) struct Holder {
    /// 任务会话的根 conversation（与 `Decision::target` 同一个口径）。
    pub conversation: ConversationId,
    /// 被停下了：消息不直通，交给 Router（Router 判 `continue` 才会清掉已停）。
    pub stopped: bool,
}

/// `line`（会话 id）派出的任务里，拥有 `position` 的那个；没有归属返回 `None`。归档的任务会话不算
/// （只扫 `active`），所以"会话还活着"不用另查。
pub(super) async fn holder_of(
    core: &DurableCore,
    line: &str,
    position: &ChannelPeer,
) -> Result<Option<Holder>, SessionError> {
    if position.thread.is_none() {
        return Ok(None);
    }
    let want = position.to_string();
    let query = SessionQuery {
        kind: Some(SessionKind::Task),
        lifecycle: Some(SessionLifecycle::Active),
        order: Some(ScanOrder::Descending),
        ..SessionQuery::default()
    };
    let mut cursor: Option<SessionCursor> = None;
    loop {
        let page = core
            .storage
            .scan_sessions(&query, 256, cursor.as_ref())
            .await?;
        for record in page.items {
            if record.parent_session.as_deref() != Some(line) {
                continue;
            }
            let Some(doc) = task_doc(&*core.storage, record.root_conversation).await? else {
                continue;
            };
            if doc.thread.as_deref() == Some(want.as_str()) {
                return Ok(Some(Holder {
                    conversation: record.root_conversation,
                    stopped: doc.stopped,
                }));
            }
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(None),
        }
    }
}

/// 任务会话 `target` 是不是这条线派出的（群里再要求回话去处的聊天与群一致）。`stop` / `continue` 执行时
/// 再校验一次：候选里本来只有这些任务，这是第二道。
pub(super) async fn belongs(
    core: &DurableCore,
    line: &str,
    group: Option<&ChannelPeer>,
    target: ConversationId,
) -> Result<bool, SessionError> {
    let Some(conversation) = core.storage.conversation(target).await? else {
        return Ok(false);
    };
    let Some(session) = conversation.session else {
        return Ok(false);
    };
    let Some(session) = core.storage.session(&session).await? else {
        return Ok(false);
    };
    if session.kind != SessionKind::Task || session.parent_session.as_deref() != Some(line) {
        return Ok(false);
    }
    let Some(group) = group else {
        return Ok(true);
    };
    let reply_chat = task_doc(&*core.storage, session.root_conversation)
        .await?
        .and_then(|doc| doc.reply)
        .and_then(|reply| reply.peer)
        .and_then(|peer| ChannelPeer::parse(&peer))
        .map(|peer| (peer.platform, peer.chat_id));
    Ok(reply_chat == Some((group.platform.clone(), group.chat_id.clone())))
}
