//! 给会话起标题的看客（`docs/storage-durable-d5.md` §4 W3a / §8）。
//!
//! 它原来还负责把 Run 的终态回给聊天渠道（`deliveries`），聊天渠道删除后（`docs/redesign.md`
//! P17）只剩一件事：**一个 Session 一个看客**（[`ensure`]），订阅 `EventHub` 里这个 Session
//! 的投影帧，对每条 run 的 `RunCompleted` 终态帧起一次标题（[`titles::spawn`]；`NO_REPLY`
//! 记号不拿来起标题）。标题只管普通会话的第一轮，换不换由 `titles` 自己判断。
//!
//! # 重启之后
//!
//! 看客只活在内存里。[`reattach`]（`service::start` 调）给每个非任务的会话挂上看客，对每个
//! **未结算**的输入 submission 起一个等它结算的小任务，结算后按落盘的结局起标题；
//! [`replay_recent`]（`background::spawn` 调）再把**最近结算**的输入起一遍标题——终态帧在
//! Gateway 起来之前就发出去的、或者在 feed 订阅生效之前落下的，不靠帧也补得上。`titles` 只在
//! 标题还是首行时才换，所以重复调用无害。任务会话（`kind = task`）不挂：它们的标题不起。

use std::sync::{Arc, Weak};
use std::time::Duration;

use komo_kernel::durable::storage::SubmissionQuery;
use komo_kernel::durable::{
    ContentBlock, Cursor, EntryId, InputState, Message, ScanOrder, SessionKind, SubmissionId,
    SubmissionKind, SubmissionRecord, SubmissionStatus, legacy_ids,
};
use komo_kernel::events::EventPayload;
use komo_kernel::protocol::sse::SseEvent;
use komo_kernel::types::chat::is_silent_reply;
use komo_kernel::types::ids::{RunId, SessionId};

use super::super::DurableCore;
use crate::service::state::GatewayState;

/// 重启时回头再收尾的、最近结算的输入个数。
const REPLAY_RECENT: usize = 64;

/// 看客轮询"状态是不是已经被丢掉"的间隔（`Weak` 升不上来就退出）。
const POLL: Duration = Duration::from_millis(500);

/// 输入 submission 放置成的 entry（还没放置过 / 已撤回就没有）。
fn placed_entry(record: &SubmissionRecord) -> Option<EntryId> {
    match &record.kind {
        SubmissionKind::Input { state } => match state {
            InputState::Placed { entry } | InputState::Done { entry, .. } => Some(*entry),
            InputState::Unanswered { entry, .. } => *entry,
            InputState::Queued => None,
        },
        SubmissionKind::Write { .. } => None,
    }
}

/// 一条 run（首个输入 submission）放置成的 entry（读不出来、还没放置过 / 已撤回就没有）。
pub async fn input_entry(core: &DurableCore, first: SubmissionId) -> Option<EntryId> {
    let Ok(Some(record)) = core.storage.submission(first).await else {
        return None;
    };
    placed_entry(&record)
}

/// 给这个 Session 挂上看客（幂等：已经有就什么都不做）。**要在提交输入之前调**：订阅之后才发生的
/// 帧才收得到，先提交后订阅会错过跑得快的那条 run 的终态。
pub fn ensure(state: &Arc<GatewayState>, session: &str) {
    let marker = RunId::from_raw(format!("session-watch:{session}"));
    if !state.start_watching(&marker) {
        return;
    }
    let mut events = state.hub.subscribe(&SessionId::from_raw(session));
    let weak: Weak<GatewayState> = Arc::downgrade(state);
    let session = session.to_owned();
    tokio::spawn(async move {
        loop {
            let received = tokio::time::timeout(POLL, events.recv()).await;
            let Some(state) = weak.upgrade() else { break };
            let frame = match received {
                Err(_) => continue,
                Ok(Ok(frame)) => frame,
                // 落后了就接着收：内存通知丢了不丢数据，重启补挂那一路兜得住已落盘的终态。
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(_)) => break,
            };
            let SseEvent::Event(event) = &frame.event else {
                continue;
            };
            let Some(run) = event.run.clone() else {
                continue;
            };
            if let EventPayload::RunCompleted(body) = &event.payload {
                let text = body
                    .final_message
                    .clone()
                    .unwrap_or_else(|| "（这一轮没有文字回复）".to_owned());
                title(&state, &session, &run, &text);
            }
        }
        if let Some(state) = weak.upgrade() {
            state.stop_watching(&RunId::from_raw(format!("session-watch:{session}")));
        }
    });
}

/// 一条 run 正常完成：起标题。不说话的记号（`NO_REPLY`）不拿来起标题。
fn title(state: &Arc<GatewayState>, session: &str, run: &RunId, reply: &str) {
    if !is_silent_reply(reply) {
        super::super::sessions::titles::spawn(state, session, run, reply);
    }
}

// ------------------------------------------------------------------ 重启之后

/// 一条已结算的输入 submission 的回答正文（只有正常完成的才有）。
async fn completed_text(core: &DurableCore, record: &SubmissionRecord) -> Option<String> {
    let SubmissionKind::Input {
        state: InputState::Done { answer, .. },
    } = &record.kind
    else {
        return None;
    };
    Some(
        answer_text(core, *answer)
            .await
            .unwrap_or_else(|| "（这一轮没有文字回复）".to_owned()),
    )
}

async fn answer_text(core: &DurableCore, entry: EntryId) -> Option<String> {
    let stored = core.storage.entry(entry).await.ok()??;
    let text = stored
        .entry
        .model
        .iter()
        .flatten()
        .find_map(|message| match message {
            Message::Assistant(message) => Some(
                message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })?;
    (!text.trim().is_empty()).then_some(text)
}

/// 这条已结算的输入是不是并进了更早的输入开的 run（并进来的输入与宿主共用同一条回答 entry，
/// 宿主的 ID 更小）。并进来的没有自己的终态。
async fn is_absorbed(core: &DurableCore, record: &SubmissionRecord) -> bool {
    let SubmissionKind::Input {
        state: InputState::Done { answer, .. },
    } = &record.kind
    else {
        return false;
    };
    let query = SubmissionQuery {
        conversation: Some(record.conversation),
        status: Some(SubmissionStatus::Done),
        order: Some(ScanOrder::Ascending),
    };
    let mut cursor: Option<Cursor> = None;
    loop {
        let Ok(page) = core
            .storage
            .scan_submissions(&query, 256, cursor.as_ref())
            .await
        else {
            return false;
        };
        for other in &page.items {
            if other.id >= record.id {
                return false;
            }
            if matches!(
                &other.kind,
                SubmissionKind::Input { state: InputState::Done { answer: a, .. } } if a == answer
            ) {
                return true;
            }
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => return false,
        }
    }
}

/// 启动时重挂看客（`service::start` 调，**在 `core.resume()` 之前、feed 订阅之后**）：每个非任务的
/// 会话一个看客；每个未结算的输入 submission 一个"等它结算再收尾"的小任务。
pub async fn reattach(state: &Arc<GatewayState>) {
    let Ok(core) = state.core() else { return };
    let core = Arc::clone(core);
    let sessions = match core.list_sessions(false).await {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::warn!(%error, "读不出会话列表，重启后的看客补挂没做成");
            return;
        }
    };
    for record in &sessions {
        if record.kind != SessionKind::Task {
            ensure(state, &record.id);
        }
    }
    let mut unsettled = Vec::new();
    for status in [SubmissionStatus::Queued, SubmissionStatus::Placed] {
        let query = SubmissionQuery {
            conversation: None,
            status: Some(status),
            order: Some(ScanOrder::Ascending),
        };
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = match core
                .storage
                .scan_submissions(&query, 256, cursor.as_ref())
                .await
            {
                Ok(page) => page,
                Err(error) => {
                    tracing::warn!(%error, "读不出未结算的输入，重启后的看客补挂没做成");
                    return;
                }
            };
            unsettled.extend(
                page.items
                    .into_iter()
                    .filter(|record| matches!(record.kind, SubmissionKind::Input { .. })),
            );
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
    }
    let mut reattached = 0usize;
    for record in unsettled {
        let session = match core.storage.conversation(record.conversation).await {
            Ok(Some(conversation)) => conversation.session,
            _ => None,
        };
        let Some(session) = session else { continue };
        if !sessions
            .iter()
            .any(|row| row.id == session && row.kind != SessionKind::Task)
        {
            continue;
        }
        reattached += 1;
        let weak = Arc::downgrade(state);
        tokio::spawn(async move {
            let Some(strong) = weak.upgrade() else { return };
            let Ok(core) = strong.core().cloned() else {
                return;
            };
            drop(strong);
            let Ok(settled) = core.wait_submission(record.id).await else {
                return;
            };
            let Some(state) = weak.upgrade() else { return };
            if is_absorbed(&core, &settled).await {
                return;
            }
            if let Some(text) = completed_text(&core, &settled).await {
                title(&state, &session, &legacy_ids::run_id(settled.id), &text);
            }
        });
    }
    if reattached > 0 {
        tracing::info!(reattached, "重启后补挂了未结算输入的看客");
    }
}

/// 启动时把**最近结算**的输入再起一遍标题（`background::spawn` 调，引擎恢复之后）。
///
/// 兜的是一种窗口：结算提交已经落盘、标题还没起就崩了。只取最近 [`REPLAY_RECENT`] 条：标题紧跟在
/// 结算后面，丢的只可能是崩溃前最后几条；`titles` 只在标题还是首行时才换，重复调用无害。
pub async fn replay_recent(state: &Arc<GatewayState>) {
    let Ok(core) = state.core() else { return };
    let core = Arc::clone(core);
    let sessions = match core.list_sessions(false).await {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::warn!(%error, "读不出会话列表，重启后的标题补起没做成");
            return;
        }
    };
    let query = SubmissionQuery {
        conversation: None,
        status: Some(SubmissionStatus::Done),
        order: Some(ScanOrder::Descending),
    };
    let Ok(page) = core
        .storage
        .scan_submissions(&query, REPLAY_RECENT, None)
        .await
    else {
        return;
    };
    for record in page.items {
        if !matches!(record.kind, SubmissionKind::Input { .. }) {
            continue;
        }
        let session = match core.storage.conversation(record.conversation).await {
            Ok(Some(conversation)) => conversation.session,
            _ => None,
        };
        let Some(session) = session else { continue };
        if !sessions
            .iter()
            .any(|row| row.id == session && row.kind != SessionKind::Task)
            || is_absorbed(&core, &record).await
        {
            continue;
        }
        if let Some(text) = completed_text(&core, &record).await {
            title(state, &session, &legacy_ids::run_id(record.id), &text);
        }
    }
}
