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
                push_unprompted(&state, &session, &run, body.final_message.as_deref()).await;
            }
        }
        if let Some(state) = weak.upgrade() {
            state.stop_watching(&RunId::from_raw(format!("session-watch:{session}")));
        }
    });
}

/// 对话里**不是操作者发起的**一轮（监听事件、任务交回之类触发的 Run）说完了话：推给订阅的设备。
/// 操作者自己发消息开始的那一轮他正看着，不推；不说话的记号与空回复不推；任务会话、定时会话、存量
/// 渠道会话不是对话（定时结果与提问走 `say_in`，那边已经推过）。
async fn push_unprompted(
    state: &Arc<GatewayState>,
    session: &str,
    run: &RunId,
    reply: Option<&str>,
) {
    let Some(text) = reply
        .map(str::trim)
        .filter(|text| !text.is_empty() && !is_silent_reply(text))
    else {
        return;
    };
    let Ok(core) = state.core() else { return };
    let Ok(record) = core.session(session).await else {
        return;
    };
    if !matches!(record.kind, SessionKind::Main | SessionKind::Normal)
        || super::super::chats::is_channel_session(&record)
        || core.run_started_by_operator(run).await
    {
        return;
    }
    state.push.notify(crate::push::PushMessage {
        title: "Komo".to_owned(),
        body: text.to_owned(),
        url: super::super::chats::say::chat_url(&record),
        tag: Some(format!("reply:{}", run.as_str())),
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

#[cfg(test)]
mod tests {
    //! 首轮跑完起标题的规则（真 Gateway、脚本化模型）。模型替身默认不起标题，这里用
    //! `auto_title(true)` 打开；起标题的那次调用是脚本里紧跟在首轮回复之后的一段。

    use std::sync::Arc;
    use std::time::Duration;

    use komo_kernel::durable::SessionRecord;
    use komo_kernel::protocol::http::SessionSummary;
    use serde_json::json;

    use super::*;
    use crate::service::test_support::harness::{
        FakeLlm, GatewayBuilder, Home, TestGateway, home_config, text_round,
    };

    /// 没有该发生的事时留给"它不该发生"的那一小段时间。
    const GRACE: Duration = Duration::from_millis(500);

    async fn record(state: &Arc<GatewayState>, session: &str) -> SessionRecord {
        state
            .core()
            .expect("Durable 核心")
            .session(session)
            .await
            .expect("会话")
    }

    async fn wait_title(state: &Arc<GatewayState>, session: &str, want: &str) {
        for _ in 0..400 {
            let row = record(state, session).await;
            if row.title.as_deref() == Some(want) && !row.title_provisional {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let row = record(state, session).await;
        panic!(
            "等了 20 秒标题也没有换成「{want}」：{:?} provisional={}",
            row.title, row.title_provisional
        );
    }

    /// 网页开一条支线会话（`POST /v1/sessions`）。
    async fn side_session(gw: &TestGateway, body: serde_json::Value) -> String {
        let (code, body) = gw.post("/v1/sessions", body).await;
        assert_eq!(code, 200, "{body}");
        serde_json::from_str::<SessionSummary>(&body)
            .expect("会话摘要")
            .session
            .to_string()
    }

    async fn gateway(llm: &Arc<FakeLlm>) -> TestGateway {
        GatewayBuilder::new(&home_config())
            .fake_llm(Arc::clone(llm))
            .auto_title(true)
            .start()
            .await
    }

    #[tokio::test]
    async fn a_side_session_gets_a_model_title_after_its_first_run() {
        let llm = FakeLlm::new(vec![
            vec![text_round(1, "已经修好了")],
            vec![text_round(1, "标题：《排查 shell 报错》。")],
        ]);
        let gw = gateway(&llm).await;
        let session = side_session(&gw, json!({})).await;

        let receipt = gw
            .say_in(
                &SessionId::from_raw(&session),
                "帮我看看这个报错",
                "k1",
                false,
            )
            .await;
        gw.wait_run(&receipt.run).await;

        wait_title(gw.state(), &session, "排查 shell 报错").await;
        assert_eq!(llm.turns(), 2, "一次主回复、一次起标题");
    }

    #[tokio::test]
    async fn a_session_with_a_real_title_keeps_it() {
        let llm = FakeLlm::always(vec![text_round(1, "好的")]);
        let gw = gateway(&llm).await;
        let session = side_session(&gw, json!({ "title": "我自己起的名字" })).await;

        let receipt = gw
            .say_in(
                &SessionId::from_raw(&session),
                "帮我看看这个报错",
                "k1",
                false,
            )
            .await;
        gw.wait_run(&receipt.run).await;
        tokio::time::sleep(GRACE).await;

        let row = record(gw.state(), &session).await;
        assert_eq!(row.title.as_deref(), Some("我自己起的名字"));
        assert!(!row.title_provisional);
        assert_eq!(llm.turns(), 1, "只有主回复，没有起标题那一次");
    }

    #[tokio::test]
    async fn the_title_is_set_once_and_later_runs_leave_it_alone() {
        let llm = FakeLlm::new(vec![
            vec![text_round(1, "第一轮回复")],
            vec![text_round(1, "头一个标题")],
            vec![text_round(1, "第二轮回复")],
        ]);
        let gw = gateway(&llm).await;
        let session = side_session(&gw, json!({})).await;
        let id = SessionId::from_raw(&session);

        let first = gw.say_in(&id, "第一件事", "k1", false).await;
        gw.wait_run(&first.run).await;
        wait_title(gw.state(), &session, "头一个标题").await;

        let second = gw.say_in(&id, "第二件事", "k2", false).await;
        gw.wait_run(&second.run).await;
        tokio::time::sleep(GRACE).await;

        let row = record(gw.state(), &session).await;
        assert_eq!(row.title.as_deref(), Some("头一个标题"));
        assert_eq!(llm.turns(), 3, "两次主回复加一次起标题，第二轮不再起");
    }

    #[tokio::test]
    async fn the_main_conversation_is_never_titled() {
        let llm = FakeLlm::always(vec![text_round(1, "好的")]);
        let gw = gateway(&llm).await;
        let main = gw.home_session().await;
        let before = record(gw.state(), main.as_str()).await;
        assert_eq!(before.kind, SessionKind::Main);

        let receipt = gw.say_in(&main, "随便聊聊", "k1", false).await;
        gw.wait_run(&receipt.run).await;
        tokio::time::sleep(GRACE).await;

        let row = record(gw.state(), main.as_str()).await;
        assert_ne!(row.title.as_deref(), Some("好的"));
        assert!(row.title_provisional, "主对话的临时标题不会被换成正式的");
        assert_eq!(llm.turns(), 1, "只有主回复，没有起标题那一次");
    }

    #[tokio::test]
    async fn a_run_settled_before_a_restart_is_titled_once_after_it() {
        let home = Home::with_config(&home_config());

        // 上一代进程：标题没开，首轮结算了、标题还是临时的首行。
        let first = FakeLlm::finisher("已经修好了");
        let gw = home.start(Arc::clone(&first)).await;
        let (code, body) = gw.post("/v1/sessions", json!({})).await;
        assert_eq!(code, 200, "{body}");
        let session = serde_json::from_str::<SessionSummary>(&body)
            .expect("会话摘要")
            .session;
        let receipt = gw.submit(&session, "k1", "帮我看看这个报错").await;
        gw.wait_terminal(&receipt.run).await;
        let before = record(gw.state(), session.as_str()).await;
        assert_eq!(before.title.as_deref(), Some("帮我看看这个报错"));
        assert!(before.title_provisional);
        gw.stop().await;

        // 下一代：重启补起（`replay_recent`），只起一次。
        home.auto_title(true);
        let second = FakeLlm::new(vec![vec![text_round(1, "排查报错")]]);
        let gw = home.start(Arc::clone(&second)).await;
        wait_title(gw.state(), session.as_str(), "排查报错").await;

        replay_recent(gw.state()).await;
        tokio::time::sleep(GRACE).await;
        assert_eq!(second.turns(), 1, "补起一次，再回放也不重复");
        gw.stop().await;
    }
}
