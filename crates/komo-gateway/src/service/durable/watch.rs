//! Run 跑起来之后谁在看：`run_watch` 的 Durable 版（`docs/storage-durable-d5.md` §4 W3a / §8）。
//!
//! Legacy 一个 Run 一个看客、订阅那个 Session 的事件流。这里**一个 Session 一个看客**
//! （[`ensure`]）：它订阅 `EventHub` 里这个 Session 的投影帧（W2b 的 feed 推进来的），对每条 run 的
//! 终态帧做一次收尾——run 是谁的由**这条 run 首个输入**说了算：
//!
//! | 首个输入 | 回给谁 |
//! |---|---|
//! | 聊天消息（`chat.input`，`data.peer`） | 那个 peer |
//! | 内部输入（`task.result` / `monitor.event` / `monitor.end`，没有 peer） | 决定 6：当前路径上最近一条带 peer 的用户输入的 peer；没有就退回 home chat（只给主会话——别的会话没有"退回去"的理由，TUI / HTTP 自己看着事件流） |
//! | TUI / HTTP / cron 的输入 | 不投（TUI 看事件流，cron 归 W3b 的 `cron_watch`） |
//!
//! 并进别的 run 的输入（steer）有自己的 `RunAbsorbed` 帧：来源是聊天消息就把"已并入"告诉那个渠道
//! （飞书那张"处理中"卡片靠它收尾）；它没有自己的终态，回复由宿主 run 的终态投出去。
//!
//! 投递仍是先写 `deliveries` 后发、按 `DeliveryId` 幂等；这里再加一层**逻辑键**
//! （`run-finished:{run}`），所以帧路径与重启补挂（下面）对同一条 run 各收尾一次也只投一条。
//!
//! # 重启之后
//!
//! 看客只活在内存里。[`reattach`]（`background::spawn` 调）做三件事：给每个非任务的会话挂上
//! 看客；对每个**未结算**的输入 submission 起一个等它结算的小任务，结算后按落盘的结局收尾；
//! 再把**最近结算**的那些输入收尾一遍（逻辑键幂等，投过的不会再投）——终态帧在 Gateway 起来之前
//! 就发出去的、或者在 feed 订阅生效之前落下的，不靠帧也补得上。
//! 任务会话（`kind = task`）不挂：它们的结果由 runtime 在子 run 收尾的同一次提交里交回父会话，
//! 再由父会话的看客按上表投出去。

use std::sync::{Arc, Weak};
use std::time::Duration;

use komo_kernel::durable::storage::SubmissionQuery;
use komo_kernel::durable::{
    ContentBlock, Cursor, EntryId, EntryRecord, InputState, Message, ScanOrder, SessionKind,
    SubmissionId, SubmissionKind, SubmissionRecord, SubmissionStatus, legacy_ids,
};
use komo_kernel::events::EventPayload;
use komo_kernel::protocol::sse::SseEvent;
use komo_kernel::types::chat::{ChannelPeer, DeliveryTarget, Outbound};
use komo_kernel::types::ids::{RunId, SessionId};

use super::DurableCore;
use super::tree::load_tree;
use crate::service::state::GatewayState;

/// 聊天输入 entry 的 kind（`data {peer, principal}`）。
pub const CHAT_INPUT: &str = "chat.input";
/// 内部输入 entry 的 kind：runtime 交回任务结果、监听事件与监听结束时写的。
const INTERNAL_INPUTS: [&str; 3] = ["task.result", "monitor.event", "monitor.end"];

/// 重启时回头再收尾的、最近结算的输入个数（每种结局）。
const REPLAY_RECENT: usize = 64;

/// 看客轮询"状态是不是已经被丢掉"的间隔（`Weak` 升不上来就退出）。
const POLL: Duration = Duration::from_millis(500);

/// 一条输入是谁的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// 聊天消息，来自这个 peer。
    Peer(ChannelPeer),
    /// runtime 的内部输入（任务结果、监听事件）：来源对端要按决定 6 去路径上找。
    Internal,
    /// TUI / HTTP / cron，或者认不出来（输入从没放置过）：这里不投。
    Other,
}

/// 一条 entry 的来源。
pub(super) fn source_of_entry(entry: &EntryRecord) -> Source {
    if entry.kind == CHAT_INPUT {
        return entry
            .data
            .as_ref()
            .and_then(|data| data.get("peer"))
            .and_then(|peer| peer.as_str())
            .and_then(ChannelPeer::parse)
            .map_or(Source::Other, Source::Peer);
    }
    if INTERNAL_INPUTS.contains(&entry.kind.as_str()) {
        Source::Internal
    } else {
        Source::Other
    }
}

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

/// 一条 run（首个输入 submission）的来源。读不出来按 [`Source::Other`]：宁可不投，不投错人。
pub async fn source_of(core: &DurableCore, first: SubmissionId) -> (Source, Option<EntryId>) {
    let Ok(Some(record)) = core.storage.submission(first).await else {
        return (Source::Other, None);
    };
    let Some(entry) = placed_entry(&record) else {
        return (Source::Other, None);
    };
    match core.storage.entry(entry).await {
        Ok(Some(stored)) => (source_of_entry(&stored.entry), Some(entry)),
        _ => (Source::Other, Some(entry)),
    }
}

/// 决定 6：内部输入 `entry` 所在路径上、它之前最近一条带 peer 的用户输入的 peer。
pub async fn peer_on_path(
    core: &DurableCore,
    session: &str,
    entry: EntryId,
) -> Option<ChannelPeer> {
    let tree = load_tree(&*core.storage, session).await.ok()?;
    tree.path(entry)
        .into_iter()
        .rev()
        .filter(|record| record.id != entry)
        .find_map(|record| match source_of_entry(record) {
            Source::Peer(peer) => Some(peer),
            _ => None,
        })
}

/// 一条 run 的结果该回给谁。`Some(None)` = 退回 home chat，`None` = 不投。
async fn destination(
    core: &DurableCore,
    session: &str,
    first: SubmissionId,
) -> Option<Option<ChannelPeer>> {
    match source_of(core, first).await {
        (Source::Peer(peer), _) => Some(Some(peer)),
        (Source::Internal, Some(entry)) => match peer_on_path(core, session, entry).await {
            Some(peer) => Some(Some(peer)),
            None => {
                // 没有人说过话：只有主会话退回 home chat。
                let record = core.storage.session(session).await.ok()??;
                (record.kind == SessionKind::Main).then_some(None)
            }
        },
        _ => None,
    }
}

/// run 的结局。
#[derive(Debug, Clone)]
enum Outcome {
    Completed(String),
    Failed(String),
    Cancelled,
}

impl Outcome {
    /// 投出去的正文（与 Legacy 同一套话）。
    fn text(&self) -> String {
        match self {
            Outcome::Completed(text) => text.clone(),
            Outcome::Failed(reason) => format!("任务失败：{reason}"),
            Outcome::Cancelled => "任务已取消。".to_owned(),
        }
    }
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
            match &event.payload {
                EventPayload::RunCompleted(body) => {
                    let text = body
                        .final_message
                        .clone()
                        .unwrap_or_else(|| "（这一轮没有文字回复）".to_owned());
                    finish(&state, &session, &run, Outcome::Completed(text)).await;
                }
                EventPayload::RunFailed(body) => {
                    finish(&state, &session, &run, Outcome::Failed(body.reason.clone())).await;
                }
                EventPayload::RunCancelled(_) => {
                    finish(&state, &session, &run, Outcome::Cancelled).await;
                }
                EventPayload::RunAbsorbed(body) => {
                    absorbed(&state, &session, &run, &body.into).await;
                }
                _ => {}
            }
        }
        if let Some(state) = weak.upgrade() {
            state.stop_watching(&RunId::from_raw(format!("session-watch:{session}")));
        }
    });
}

/// 并进了正在跑的那条（§6 插话）：来源是聊天消息就把"已并入"投给它（飞书那张"处理中"卡片靠它
/// 收尾，别的渠道忽略）。内部输入并进去没有人在等一句"已并入"。
async fn absorbed(state: &Arc<GatewayState>, session: &str, run: &RunId, into: &RunId) {
    let Ok(core) = state.core() else { return };
    let Some(first) = legacy_ids::submission_of(run) else {
        return;
    };
    let (Source::Peer(peer), _) = source_of(core, first).await else {
        return;
    };
    if let Err(error) = state
        .notifier
        .log()
        .deliver_once(
            &DeliveryTarget::to_peer(peer),
            Outbound::RunAbsorbed {
                session: SessionId::from_raw(session),
                run: run.clone(),
                into: into.clone(),
            },
            &format!("run-absorbed:{run}"),
        )
        .await
    {
        tracing::warn!(%error, %run, "并入通知投不出去");
    }
}

/// 一条 run 收尾：起标题（正常完成的第一轮），再把结果投回去。
async fn finish(state: &Arc<GatewayState>, session: &str, run: &RunId, outcome: Outcome) {
    let Ok(core) = state.core() else { return };
    let Some(first) = legacy_ids::submission_of(run) else {
        return;
    };
    if let Outcome::Completed(text) = &outcome {
        super::titles::spawn(state, session, run, text);
    }
    let Some(destination) = destination(core, session, first).await else {
        // TUI / HTTP 自己在看事件流（§13.1），cron 归它自己的看客。
        return;
    };
    let message = Outbound::RunFinished {
        session: SessionId::from_raw(session),
        run: run.clone(),
        summary: outcome.text(),
    };
    let key = format!("run-finished:{run}");
    let result = match destination {
        Some(peer) => state
            .notifier
            .log()
            .deliver_once(&DeliveryTarget::to_peer(peer), message, &key)
            .await
            .map(|_| ()),
        None => state
            .notifier
            .deliver_home_once(message, &key)
            .await
            .map(|_| ()),
    };
    if let Err(error) = result {
        tracing::warn!(%error, %run, "回复投不出去");
    }
}

// ------------------------------------------------------------------ 重启之后

/// 一条已结算的输入 submission 的结局。
async fn outcome_of(core: &DurableCore, record: &SubmissionRecord) -> Option<Outcome> {
    let SubmissionKind::Input { state } = &record.kind else {
        return None;
    };
    match state {
        InputState::Done { answer, .. } => {
            let text = answer_text(core, *answer).await;
            Some(Outcome::Completed(
                text.unwrap_or_else(|| "（这一轮没有文字回复）".to_owned()),
            ))
        }
        InputState::Unanswered { reason, detail, .. } => {
            if matches!(reason.as_str(), "aborted" | "reset") {
                return Some(Outcome::Cancelled);
            }
            let mut text = reason.clone();
            match detail {
                Some(serde_json::Value::String(detail)) if !detail.is_empty() => {
                    text = format!("{text}: {detail}");
                }
                Some(serde_json::Value::Null) | None => {}
                Some(detail) => text = format!("{text}: {detail}"),
            }
            Some(Outcome::Failed(text))
        }
        InputState::Queued | InputState::Placed { .. } => None,
    }
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
            if let Some(outcome) = outcome_of(&core, &settled).await {
                finish(&state, &session, &legacy_ids::run_id(settled.id), outcome).await;
            }
        });
    }
    if reattached > 0 {
        tracing::info!(reattached, "重启后补挂了未结算输入的看客");
    }
}

/// 启动时把**最近结算**的输入收尾一遍（`background::spawn` 调，引擎恢复之后）。
///
/// # 为什么还要它，以及为什么还是"最近 N 条"
///
/// W3a 的第一版靠它兜两种丢帧：feed 订阅之前落下的终态帧、和引擎恢复到 feed 订阅之间产生的。W4a
/// 把 feed 订阅与 [`reattach`] 都提到 `core.resume()` 之前，这两种不再存在——崩溃时在飞的 run 在
/// 恢复里结算时，feed 与看客都已就位。剩下的只有一种窗口：**结算提交已经落盘、投递记录还没写**就崩了
/// （上一代进程里 feed → hub → 看客 → `deliver_once` 的那几毫秒）。
///
/// 要把这个窗口判准，得知道"哪些结算过的输入还没有投递记录"：`deliveries` 在 `aux.db`、没有按逻辑键
/// 的查询（`record_once` 是 O(该 peer 的行) 的扫描），结算时刻也不在 submission 上；要精确就得加
/// 一个投递索引或结算水位线，动 aux 表或 runtime，超出 W4a。所以**决定保留"最近 N 条"**：投递紧跟在
/// 结算后面，丢的只可能是崩溃前最后几条；`deliver_once` 幂等，扫多了只是多几次查找。N 仍取
/// [`REPLAY_RECENT`]（每种结局各取）。D7 之后若 aux 表合进 `durable.db`，可以改成"结算与投递记录同提交"，
/// 这个窗口就不存在了（设计里推迟的"同库同提交"，决定 4）。
pub async fn replay_recent(state: &Arc<GatewayState>) {
    let Ok(core) = state.core() else { return };
    let core = Arc::clone(core);
    let sessions = match core.list_sessions(false).await {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::warn!(%error, "读不出会话列表，重启后的补投没做成");
            return;
        }
    };
    for status in [SubmissionStatus::Done, SubmissionStatus::Unanswered] {
        let query = SubmissionQuery {
            conversation: None,
            status: Some(status),
            order: Some(ScanOrder::Descending),
        };
        let Ok(page) = core
            .storage
            .scan_submissions(&query, REPLAY_RECENT, None)
            .await
        else {
            continue;
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
            if let Some(outcome) = outcome_of(&core, &record).await {
                finish(state, &session, &legacy_ids::run_id(record.id), outcome).await;
            }
        }
    }
}
