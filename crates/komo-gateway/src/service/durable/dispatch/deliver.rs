//! 主线上的回复投到渠道（`docs/tasks.md` §4.12 U3、§4.14 F2）。
//!
//! 任务的回复与 `list` 的清单都落成主会话里一条 `main.reply` 展示 entry（`data.peer` 写定去处）。
//! 这里订阅提交，看到这种 entry 就原文投给那个对端——先写 `deliveries` 后发，按 entry 去重：订阅
//! 与重启补投（[`reattach`]）对同一条各投一次也只发一条。没有 `peer`（HTTP 来的）只留在主线上。
//!
//! 投的是 `Outbound::RunFinished`（run = 被回答的那条主线消息）：渠道把它渲染成纯文本，飞书靠它把
//! 那条消息的"处理中"卡片换成回复；正文原样，不加前缀、不带任务号。

use std::sync::Arc;

use komo_kernel::durable::storage::{EntryQuery, SessionQuery};
use komo_kernel::durable::{
    EntryRecord, ScanOrder, SessionKind, SessionLifecycle, StorageWrite, SubmissionId, legacy_ids,
};
use komo_kernel::traits::Shutdown;
use komo_kernel::types::chat::{ChannelPeer, DeliveryTarget, Outbound};
use komo_kernel::types::ids::SessionId;
use tokio::sync::mpsc;

use super::MAIN_REPLY_ENTRY;
use crate::service::state::GatewayState;

/// 重启时回头补投的最近几条（逻辑键幂等，投过的不会再发）。
const REPLAY_RECENT: usize = 64;

/// 订阅提交、起投递任务。订阅在引擎调度开始之前接上（`service::start`），之后落下的回复一条不漏。
pub fn spawn(state: &Arc<GatewayState>, shutdown: &Shutdown) {
    let Ok(core) = state.core() else { return };
    let (sender, mut receiver) = mpsc::unbounded_channel::<EntryRecord>();
    let subscription = core.engine.line().subscribe_commits(move |publication| {
        // 回调在提交线上同步执行：只把关心的 entry 克隆出去。
        for write in &publication.writes {
            if let StorageWrite::Entry(entry) = write
                && entry.kind == MAIN_REPLY_ENTRY
            {
                let _ = sender.send(entry.clone());
            }
        }
    });
    let weak = Arc::downgrade(state);
    let shutdown = shutdown.clone();
    tokio::spawn(async move {
        loop {
            let next = tokio::select! {
                next = receiver.recv() => next,
                _ = async {
                    while !shutdown.is_cancelled() {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                } => None,
            };
            let Some(entry) = next else { break };
            let Some(state) = weak.upgrade() else { break };
            deliver(&state, &entry).await;
        }
        subscription.dispose();
    });
}

/// 启动时补投：每个主会话当前分支上最近的几条回复各过一遍（崩在"回复落下之后、投出之前"的靠这个补）。
pub async fn reattach(state: &Arc<GatewayState>) {
    let Ok(core) = state.core() else { return };
    let query = SessionQuery {
        kind: Some(SessionKind::Main),
        lifecycle: Some(SessionLifecycle::Active),
        ..SessionQuery::default()
    };
    let mut cursor = None;
    loop {
        let page = match core
            .storage
            .scan_sessions(&query, 64, cursor.as_ref())
            .await
        {
            Ok(page) => page,
            Err(error) => {
                tracing::warn!(%error, "读不出主会话，重启后的回复补投没做成");
                return;
            }
        };
        for session in &page.items {
            let entries = EntryQuery {
                order: Some(ScanOrder::Descending),
                ..EntryQuery::new(session.current_branch)
            };
            let Ok(recent) = core
                .storage
                .scan_entries(&entries, REPLAY_RECENT, None)
                .await
            else {
                continue;
            };
            // 旧的先投。
            for entry in recent.items.iter().rev() {
                if entry.kind == MAIN_REPLY_ENTRY {
                    deliver(state, entry).await;
                }
            }
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
}

/// 投一条回复。没有对端的不投；投不出去（渠道没起来）留在 `deliveries` 里等补发。
pub(super) async fn deliver(state: &Arc<GatewayState>, entry: &EntryRecord) {
    let Some(data) = &entry.data else { return };
    let Some(peer) = data
        .get("peer")
        .and_then(|peer| peer.as_str())
        .and_then(ChannelPeer::parse)
    else {
        return;
    };
    let text = data
        .get("text")
        .and_then(|text| text.as_str())
        .unwrap_or_default();
    let session = data
        .get("session")
        .and_then(|session| session.as_str())
        .unwrap_or_default();
    let run = data
        .pointer("/origin/submission")
        .and_then(|id| serde_json::from_value::<SubmissionId>(id.clone()).ok())
        .map(legacy_ids::run_id)
        .unwrap_or_else(|| legacy_ids::run_id(SubmissionId::new(entry.id.get())));
    let message = Outbound::RunFinished {
        session: SessionId::from_raw(session),
        run,
        summary: text.to_owned(),
    };
    let key = format!("main-reply:{}", entry.id);
    if let Err(error) = state
        .notifier
        .log()
        .deliver_once(&DeliveryTarget::to_peer(peer), message, &key)
        .await
    {
        tracing::warn!(%error, entry = %entry.id, "主线上的回复投不出去");
    }
}
