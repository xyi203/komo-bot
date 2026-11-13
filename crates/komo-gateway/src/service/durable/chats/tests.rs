//! 对话（`docs/redesign.md` §2.1）：列表、投一句话、停住的运行在对话里开口。走真 Gateway（真数据目录、
//! 真 durable.db）与脚本化模型。

use std::sync::Arc;

use komo_kernel::durable::legacy_events::MAIN_REPLY_ENTRY;
use komo_kernel::durable::storage::EntryQuery;
use komo_kernel::durable::{ScanOrder, SessionKind, SessionRecord};
use komo_kernel::events::EventPayload;
use komo_kernel::protocol::http::{ChatListResponse, SessionSummary};
use komo_kernel::types::ids::SessionId;
use komo_runtime::durable::harness::Harness;
use serde_json::{Value, json};

use super::super::DurableCore;
use super::super::sessions::NewSession;
use super::say::{Said, Say, SaySource};
use super::{Route, WEB_SOURCE};
use crate::service::test_support::Gate;
use crate::service::test_support::harness::{FakeLlm, Gw, Home, call_round, text_round};

fn core_of(gw: &Gw) -> Arc<DurableCore> {
    Arc::clone(gw.state().core().expect("Durable 核心"))
}

async fn wait_for<T, F, Fut>(what: &str, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    for _ in 0..400 {
        if let Some(found) = probe().await {
            return found;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("等了 20 秒也没有：{what}");
}

/// 一段对话里"Komo 说的话"（`main.reply` entry 的 `data`），旧的在前。
async fn says(core: &DurableCore, session: &str) -> Vec<Value> {
    let record = core.session(session).await.expect("会话");
    let mut query = EntryQuery::new(record.current_branch);
    query.order = Some(ScanOrder::Ascending);
    core.storage
        .scan_entries(&query, 1000, None)
        .await
        .expect("读 entry")
        .items
        .into_iter()
        .filter(|entry| entry.kind == MAIN_REPLY_ENTRY)
        .filter_map(|entry| entry.data)
        .collect()
}

async fn chats(gw: &Gw) -> ChatListResponse {
    let (code, body) = gw.get("/v1/chats").await;
    assert_eq!(code, 200, "{body}");
    serde_json::from_str(&body).expect("对话列表")
}

/// 让这个 Session 当前分支的下一条输入 fault（`komo.agent` 写成读不出来的值，钩子 `setup` 失败 →
/// 生成任务 faulted），它就成了清单里一条 `blocked` 的介入。
async fn poison(core: &DurableCore, session: &str) {
    use super::super::sessions::agent_doc::{AGENT_DOC_VERSION, address};
    let branch = core.session(session).await.expect("会话").current_branch;
    core.engine
        .commit(move |tx| {
            Box::pin(async move {
                tx.put_document(
                    address(branch),
                    AGENT_DOC_VERSION,
                    json!("坏掉的 komo.agent"),
                )
                .await
            })
        })
        .await
        .expect("写坏 komo.agent");
}

/// 开一段支线对话（`POST /v1/sessions`），说一句话，等这一轮收尾。
async fn side_chat(gw: &Gw, key: &str, text: &str) -> SessionId {
    let session = gw.open_session().await;
    let run = gw.submit(&session, key, text).await.run;
    gw.wait_terminal(&run).await;
    session
}

/// 让 `session` 里出一条停住的运行：先坏掉 `komo.agent`，再发一条输入。返回那条介入的 handle。
async fn stall(gw: &Gw, session: &SessionId, key: &str) -> String {
    poison(&core_of(gw), session.as_str()).await;
    let run = gw.submit(session, key, "跑一下").await.run;
    gw.wait_intervention(&run).await.handle
}

// ------------------------------------------------------------------ 支线对话

/// 支线对话就是 `POST /v1/sessions` 建的普通会话：`kind = normal`、`source = web`，第一句话成为标题，
/// 消息直通（一条输入一个 Run，没有别的分发）。
#[tokio::test]
async fn a_side_chat_is_a_normal_session_from_the_web_titled_by_its_first_words() {
    let home = Home::new();
    let llm = FakeLlm::finisher("好");
    let gw = home.start(Arc::clone(&llm)).await;
    let session = gw.open_session().await;
    let core = core_of(&gw);
    let record = core.session(session.as_str()).await.unwrap();
    assert_eq!(record.kind, SessionKind::Normal);
    assert_eq!(record.source.as_deref(), Some(WEB_SOURCE));

    let run = gw
        .submit(&session, "side-1", "帮我看看明天的天气")
        .await
        .run;
    gw.wait_terminal(&run).await;
    let record = core.session(session.as_str()).await.unwrap();
    assert_eq!(record.title.as_deref(), Some("帮我看看明天的天气"));
    assert_eq!(llm.turns(), 1, "直通：一条输入一轮模型调用");
    gw.stop().await;
}

// ------------------------------------------------------------------ 列表

/// 主对话固定第一；其余在等你的在前，再按最后一句话倒序；渠道的存量会话不进列表。
/// 每行带标题、最后一句话（用户或 Komo 说的，投进来的也算）、时间、在等你、在跑；`waitingCount` 是
/// 等你的事的总数。
#[tokio::test]
async fn the_chat_list_puts_main_first_then_waiting_then_newest_with_its_fields() {
    let home = Home::new();
    let gw = home.start(FakeLlm::finisher("好的，记下了")).await;
    let core = core_of(&gw);

    let oldest = side_chat(&gw, "a", "第一段").await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let stalled = side_chat(&gw, "b", "第二段").await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let newest = side_chat(&gw, "c", "第三段").await;
    // 存量的渠道会话（`source = chat:`）不是对话。
    let channel = core
        .create_session(NewSession::new("assistant").source("chat:telegram:42"))
        .await
        .unwrap();
    // 第二段里停住一个运行：在等你，排到最前（主对话除外）。
    let handle = stall(&gw, &stalled, "b-2").await;

    let list = wait_for("停住的话投进支线", || async {
        let list = chats(&gw).await;
        let row = list.chats.iter().find(|row| row.id == stalled)?;
        row.last_message.contains("出错了").then_some(list)
    })
    .await;
    let ids: Vec<&str> = list.chats.iter().map(|row| row.id.as_str()).collect();
    assert!(list.chats[0].main, "主对话固定第一");
    assert_eq!(list.chats[0].title, "主对话");
    assert_eq!(
        &ids[1..],
        [stalled.as_str(), newest.as_str(), oldest.as_str()],
        "等你的在前，再按最后一句话倒序"
    );
    assert!(!ids.contains(&channel.id.as_str()), "渠道会话不进列表");
    assert_eq!(list.waiting_count, 1);

    let row = &list.chats[1];
    assert!(row.waiting && !row.main);
    assert_eq!(row.title, "第二段");
    assert!(
        row.last_message.starts_with("这段对话里有一步出错了") && row.last_message.ends_with('…'),
        "投进来的话算最后一句，并截断：{}",
        row.last_message
    );
    let quiet = &list.chats[2];
    assert!(!quiet.waiting && !quiet.running);
    assert_eq!(quiet.last_message, "好的，记下了", "最后一句是 Komo 的回复");
    assert!(list.chats[0].last_message.is_empty(), "主对话还没有话");
    assert!(
        list.chats[2].last_message_at > list.chats[3].last_message_at,
        "时间倒序"
    );
    // 答复之后不再等你。
    gw.answer(&handle, "abandon").await;
    let list = chats(&gw).await;
    assert_eq!(list.waiting_count, 0);
    assert!(list.chats.iter().all(|row| !row.waiting));
    gw.stop().await;
}

/// 长话截断成一行；对话正在跑的那一行 `running`。
#[tokio::test]
async fn a_long_last_line_is_cut_and_a_chat_with_a_live_run_is_running() {
    let gate = Gate::new();
    let home = Home::new();
    home.install_tools(vec![gate.tool()]);
    let long = "很长的回复".repeat(40);
    let llm = FakeLlm::new(vec![
        vec![text_round(1, &long)],
        vec![
            call_round(1, "pc-1", "gate", json!({})),
            text_round(2, "过了"),
        ],
    ]);
    let gw = home.start(llm).await;
    let session = side_chat(&gw, "a", "说个长的").await;
    let list = chats(&gw).await;
    let row = list.chats.iter().find(|row| row.id == session).unwrap();
    assert_eq!(
        row.last_message.chars().count(),
        super::list::PREVIEW_CHARS + 1
    );
    assert!(row.last_message.ends_with('…'));
    assert!(!row.running);

    let run = gw.submit(&session, "a-2", "过一下门").await.run;
    gate.wait_entered(1).await;
    let list = chats(&gw).await;
    assert!(
        list.chats
            .iter()
            .find(|row| row.id == session)
            .unwrap()
            .running
    );
    gate.release();
    gw.wait_terminal(&run).await;
    let list = chats(&gw).await;
    assert!(
        !list
            .chats
            .iter()
            .find(|row| row.id == session)
            .unwrap()
            .running
    );
    gw.stop().await;
}

// ------------------------------------------------------------------ 最后一句话的时间

/// 用户输入受理、Run 以最终回复结束、往对话里投一句话，三处都更新 `last_message_at`；
/// 旧记录（没有这一格，读成 0）列表回落到 `changed_at`。
#[tokio::test]
async fn last_message_at_moves_with_input_final_reply_and_a_posted_line() {
    let gate = Gate::new();
    let home = Home::new();
    home.install_tools(vec![gate.tool()]);
    let gw = home
        .start(FakeLlm::new(vec![vec![
            call_round(1, "pc-1", "gate", json!({})),
            text_round(2, "做完了"),
        ]]))
        .await;
    let core = core_of(&gw);
    let session = gw.open_session().await;
    let at = |record: SessionRecord| record.last_message_at;
    assert_eq!(
        at(core.session(session.as_str()).await.unwrap()),
        0,
        "建会话还没有话"
    );

    let run = gw.submit(&session, "t-1", "开始").await.run;
    gate.wait_entered(1).await;
    let after_input = core.session(session.as_str()).await.unwrap();
    assert!(after_input.last_message_at > 0);
    assert_eq!(
        after_input.last_message_at, after_input.changed_at,
        "同一次提交里改"
    );

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    gate.release();
    gw.wait_terminal(&run).await;
    let after_reply = at(core.session(session.as_str()).await.unwrap());
    assert!(after_reply > after_input.last_message_at, "最终回复更新它");

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let said = core
        .say_in(
            session.as_str(),
            Say {
                text: "补一句".into(),
                model_note: None,
                source: SaySource::Cron,
                key: "k-1".into(),
                data: json!({}),
            },
        )
        .await
        .unwrap();
    assert_eq!(said, Said::Posted);
    let after_say = at(core.session(session.as_str()).await.unwrap());
    assert!(after_say > after_reply, "投一句话更新它");

    // 旧记录：没有这一格 → 读成 0 → 列表回落到 changed_at。
    let mut old = core.session(session.as_str()).await.unwrap();
    old.last_message_at = 0;
    old.changed_at = 1_700_000_000_000;
    core.engine
        .commit(move |tx| {
            Box::pin(async move {
                tx.put_session(old);
                Ok(())
            })
        })
        .await
        .unwrap();
    let list = chats(&gw).await;
    let row = list.chats.iter().find(|row| row.id == session).unwrap();
    assert_eq!(row.last_message_at.unix_timestamp(), 1_700_000_000);
    gw.stop().await;
}

// ------------------------------------------------------------------ 投一句话

/// 投一句话不起 Run、网页事件流里是一条 Komo 的话、下一轮进模型上下文；同一个关联键只投一次，
/// 重启之后再投也不重复。
#[tokio::test]
async fn a_posted_line_starts_no_run_reaches_the_next_turn_and_is_posted_once_even_after_a_restart()
{
    let home = Home::new();
    let llm = FakeLlm::finisher("好");
    let gw = home.start(Arc::clone(&llm)).await;
    let core = core_of(&gw);
    let session = gw.open_session().await;
    let say = || Say {
        text: "要把这封邮件发出去吗？".into(),
        model_note: Some("（附注：handle=77）".into()),
        source: SaySource::Intervention,
        key: "intervention:confirm:77".into(),
        data: json!({ "handle": "77" }),
    };

    assert_eq!(
        core.say_in(session.as_str(), say()).await.unwrap(),
        Said::Posted
    );
    assert_eq!(
        core.say_in(session.as_str(), say()).await.unwrap(),
        Said::Duplicate
    );
    assert_eq!(llm.turns(), 0, "不启动 Run");
    let record = core.session(session.as_str()).await.unwrap();
    assert!(
        Harness::live(&*core.storage, record.current_branch)
            .await
            .unwrap()
            .run
            .is_none()
    );
    let posted = says(&core, session.as_str()).await;
    assert_eq!(posted.len(), 1, "同一个键只投一次");
    assert_eq!(posted[0]["source"], "intervention");
    assert_eq!(posted[0]["handle"], "77");
    assert_eq!(posted[0]["key"], "intervention:confirm:77");
    assert!(
        !posted[0]["text"].as_str().unwrap().contains("handle"),
        "界面上的话不带内部附注"
    );

    // 事件流里是一条 Komo 的话（不挂任何 Run）。
    let events = gw.events(&session).await;
    let lines: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::LineReply(line) => Some(line.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(lines, ["要把这封邮件发出去吗？"]);
    assert!(events.iter().all(|event| event.run.is_none()));

    // 下一轮进上下文：模型读到那句话（连同只给它看的附注）。
    let run = gw.submit(&session, "r-1", "发吧").await.run;
    gw.wait_terminal(&run).await;
    let request = llm.requests.lock().unwrap()[0].clone();
    let seen: Vec<String> = request
        .messages
        .iter()
        .filter_map(|message| message.text.clone())
        .collect();
    assert!(
        seen.iter()
            .any(|text| text.contains("要把这封邮件发出去吗？") && text.contains("handle=77")),
        "{seen:?}"
    );
    assert!(seen.last().unwrap().contains("发吧"));

    // 重启之后同一个键仍只投一次。
    gw.stop().await;
    let gw = home.start(FakeLlm::finisher("好")).await;
    let core = core_of(&gw);
    assert_eq!(
        core.say_in(session.as_str(), say()).await.unwrap(),
        Said::Duplicate
    );
    assert_eq!(says(&core, session.as_str()).await.len(), 1);
    gw.stop().await;
}

/// 对话正忙（有 Run 在跑）时投的话排进收件箱，不打断 Run，Run 收尾后落下。
#[tokio::test]
async fn a_line_posted_while_a_run_is_live_waits_for_the_boundary_and_never_interrupts_it() {
    let gate = Gate::new();
    let home = Home::new();
    home.install_tools(vec![gate.tool()]);
    let gw = home
        .start(FakeLlm::new(vec![vec![
            call_round(1, "pc-1", "gate", json!({})),
            text_round(2, "做完了"),
        ]]))
        .await;
    let core = core_of(&gw);
    let session = gw.open_session().await;
    let run = gw.submit(&session, "t-1", "开始").await.run;
    gate.wait_entered(1).await;
    core.say_in(
        session.as_str(),
        Say {
            text: "顺便说一句".into(),
            model_note: None,
            source: SaySource::Cron,
            key: "k".into(),
            data: json!({}),
        },
    )
    .await
    .unwrap();
    gate.release();
    gw.wait_terminal(&run).await;
    let posted = wait_for("忙时投的话落下", || async {
        let posted = says(&core, session.as_str()).await;
        (!posted.is_empty()).then_some(posted)
    })
    .await;
    assert_eq!(posted[0]["text"], "顺便说一句");
    gw.stop().await;
}

// ------------------------------------------------------------------ 介入投到哪

/// 介入落在哪段对话：支线里的投那段支线；主对话里的投主对话；任务会话沿 `parent_session` 往上找到第一个
/// 非任务会话；定时触发的会话投主对话；渠道的存量会话与没有父的任务会话落回主对话。
#[tokio::test]
async fn a_stalled_run_is_routed_to_the_chat_it_lives_in() {
    let home = Home::new();
    let gw = home.start(FakeLlm::finisher("好")).await;
    let core = core_of(&gw);
    let main = core.resolve_route(Route::Main).await.unwrap();
    let side = core
        .create_session(NewSession::new("assistant").source(WEB_SOURCE))
        .await
        .unwrap();
    let make = |kind: SessionKind, parent: Option<&str>, source: Option<&str>| {
        let mut spec = NewSession::new("assistant").kind(kind);
        spec.parent_session = parent.map(str::to_owned);
        if let Some(source) = source {
            spec = spec.source(source);
        }
        core.create_session(spec)
    };
    let task_in_side = make(SessionKind::Task, Some(&side.id), None).await.unwrap();
    let task_in_task = make(SessionKind::Task, Some(&task_in_side.id), None)
        .await
        .unwrap();
    let task_in_main = make(SessionKind::Task, Some(&main.id), None).await.unwrap();
    let orphan = make(SessionKind::Task, None, None).await.unwrap();
    let cron = make(SessionKind::Cron, None, Some("cron:job-1"))
        .await
        .unwrap();
    let task_in_cron = make(SessionKind::Task, Some(&cron.id), None).await.unwrap();
    let channel = make(SessionKind::Normal, None, Some("chat:feishu:ou_1"))
        .await
        .unwrap();

    let routed = |record: SessionRecord| {
        let core = Arc::clone(&core);
        let (main, side) = (main.id.clone(), side.id.clone());
        async move {
            match core.route_of(&record).await.unwrap() {
                Route::Chat(chat) if chat.id == side => "side",
                Route::Chat(chat) if chat.id == main => "main",
                Route::Chat(_) => "other",
                Route::Main => "main-fallback",
            }
        }
    };
    assert_eq!(routed(side.clone()).await, "side");
    assert_eq!(routed(main.clone()).await, "main");
    assert_eq!(routed(task_in_side).await, "side", "任务会话往上找到支线");
    assert_eq!(routed(task_in_task).await, "side", "一层一层往上");
    assert_eq!(
        routed(task_in_main).await,
        "main",
        "主对话派出的任务投主对话"
    );
    assert_eq!(routed(orphan).await, "main-fallback");
    assert_eq!(
        routed(cron.clone()).await,
        "main-fallback",
        "定时触发的投主对话"
    );
    assert_eq!(routed(task_in_cron).await, "main-fallback");
    assert_eq!(
        routed(channel).await,
        "main-fallback",
        "渠道的存量会话不是对话"
    );
    gw.stop().await;
}

/// 支线对话里停住的运行：话落在那段支线里，带 handle 与来源，说清要你决定什么、能怎么答；
/// 同一条介入不会因为重启再投一遍；主对话里停住的投主对话。
#[tokio::test]
async fn a_stalled_run_speaks_in_its_own_chat_once_even_across_a_restart() {
    let home = Home::new();
    let gw = home.start(FakeLlm::finisher("好")).await;
    let core = core_of(&gw);
    let side = side_chat(&gw, "a", "在吗").await;
    let handle = stall(&gw, &side, "a-2").await;

    let posted = wait_for("停住的话投进支线", || async {
        let posted = says(&core, side.as_str()).await;
        (!posted.is_empty()).then_some(posted)
    })
    .await;
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0]["source"], "intervention");
    assert_eq!(posted[0]["handle"], handle.as_str());
    assert_eq!(posted[0]["key"], format!("intervention:blocked:{handle}"));
    let text = posted[0]["text"].as_str().unwrap();
    assert!(text.starts_with("这段对话里有一步出错了："), "{text}");
    assert!(text.contains("你可以回复：「放弃」"), "{text}");

    // 主对话里停住的投主对话（不是支线）。
    let home_session = gw.get("/v1/home-session").await.1;
    let main = serde_json::from_str::<SessionSummary>(&home_session)
        .unwrap()
        .session;
    let main_handle = stall(&gw, &main, "m-1").await;
    let posted_main = wait_for("停住的话投进主对话", || async {
        let posted = says(&core, main.as_str()).await;
        (!posted.is_empty()).then_some(posted)
    })
    .await;
    assert_eq!(posted_main[0]["handle"], main_handle.as_str());
    assert_eq!(says(&core, side.as_str()).await.len(), 1);

    // 重启：仍未答复的介入回放一遍，每条在它的对话里仍然只有一句。
    gw.stop().await;
    let gw = home.start(FakeLlm::finisher("好")).await;
    let core = core_of(&gw);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(says(&core, side.as_str()).await.len(), 1);
    assert_eq!(says(&core, main.as_str()).await.len(), 1);
    assert_eq!(
        core.announce_intervention(&handle).await.unwrap(),
        Some(Said::Duplicate)
    );
    gw.stop().await;
}

/// 任务会话里停住的运行投到派它的那段对话。
#[tokio::test]
async fn a_stalled_task_session_speaks_in_the_chat_that_dispatched_it() {
    let home = Home::new();
    let gw = home.start(FakeLlm::finisher("好")).await;
    let core = core_of(&gw);
    let side = side_chat(&gw, "a", "派个活").await;
    let mut spec = NewSession::new("assistant").kind(SessionKind::Task);
    spec.parent_session = Some(side.as_str().to_owned());
    spec = spec.title("查空调");
    let task = SessionId::from_raw(core.create_session(spec).await.unwrap().id);
    let handle = stall(&gw, &task, "t-1").await;

    let posted = wait_for("任务的话投进派它的支线", || async {
        let posted = says(&core, side.as_str()).await;
        (!posted.is_empty()).then_some(posted)
    })
    .await;
    assert_eq!(posted[0]["handle"], handle.as_str());
    assert!(
        posted[0]["text"].as_str().unwrap().contains("后台任务"),
        "{}",
        posted[0]["text"]
    );
    assert!(
        says(&core, task.as_str()).await.is_empty(),
        "不投任务会话自己"
    );
    gw.stop().await;
}
