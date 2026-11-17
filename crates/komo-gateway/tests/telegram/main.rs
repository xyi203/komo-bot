//! Telegram 备用入口（`docs/telegram.md`）：真数据目录、真 `service::start`，Bot API 换成本机的假服务。
//!
//! 覆盖：操作者私聊进主对话并收到回复、别人的消息不理、同一个 update 重投只受理一次、命令、主动通知。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::post;
use komo_gateway::push::PushMessage;
use komo_gateway::service::test_support::harness::{
    DEFAULT_ENV, FakeLlm, Gw, Home, config_toml, text_round,
};
use serde_json::{Value, json};

const TOKEN: &str = "test-token";
const OPERATOR: i64 = 4242;

/// 假的 Bot API：`getUpdates` 交出排着的 update（**不看 offset**，所以重投也会再来一次），`sendMessage` 记下来。
#[derive(Clone, Default)]
struct FakeBot {
    queued: Arc<Mutex<Vec<Value>>>,
    sent: Arc<Mutex<Vec<(i64, String)>>>,
}

impl FakeBot {
    async fn start() -> (FakeBot, String) {
        let bot = FakeBot::default();
        let app = Router::new()
            .route(&format!("/bot{TOKEN}/getUpdates"), post(get_updates))
            .route(&format!("/bot{TOKEN}/sendMessage"), post(send_message))
            .with_state(bot.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (bot, url)
    }

    fn push(&self, update_id: i64, from: i64, text: &str) {
        self.queued.lock().unwrap().push(json!({
            "update_id": update_id,
            "message": {
                "message_id": update_id,
                "date": 0,
                "chat": { "id": from, "type": "private" },
                "from": { "id": from, "is_bot": false, "first_name": "x" },
                "text": text,
            },
        }));
    }

    fn sent(&self) -> Vec<(i64, String)> {
        self.sent.lock().unwrap().clone()
    }

    /// 等到发出去的消息里有一条满足 `want`。
    async fn wait_sent(&self, want: impl Fn(&(i64, String)) -> bool) -> (i64, String) {
        for _ in 0..200 {
            if let Some(found) = self.sent().into_iter().find(|sent| want(sent)) {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("等不到想要的消息，发出去的是：{:?}", self.sent());
    }
}

async fn get_updates(State(bot): State<FakeBot>) -> Json<Value> {
    let updates: Vec<Value> = std::mem::take(&mut *bot.queued.lock().unwrap());
    if updates.is_empty() {
        // 长轮询的样子：没有就挂一会儿，别让 Gateway 空转。
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Json(json!({ "ok": true, "result": updates }))
}

async fn send_message(State(bot): State<FakeBot>, Json(body): Json<Value>) -> Json<Value> {
    let chat = body["chat_id"].as_i64().unwrap();
    let text = body["text"].as_str().unwrap().to_owned();
    bot.sent.lock().unwrap().push((chat, text));
    Json(json!({
        "ok": true,
        "result": { "message_id": 1, "chat": { "id": chat, "type": "private" } },
    }))
}

async fn start(llm: Arc<FakeLlm>) -> (Home, Gw, FakeBot) {
    let (bot, url) = FakeBot::start().await;
    let home = Home::with_config(&config_toml(&format!(
        "[telegram]\noperator = {OPERATOR}\nendpoint = \"{url}\"\n"
    )));
    std::fs::write(
        home.path().join(".env"),
        format!("{DEFAULT_ENV}KOMO_TELEGRAM_BOT_TOKEN={TOKEN}\n"),
    )
    .unwrap();
    let gw = home.start(llm).await;
    (home, gw, bot)
}

#[tokio::test]
async fn an_operator_message_goes_to_the_main_chat_and_the_reply_comes_back() {
    let (_home, gw, bot) = start(FakeLlm::always(vec![text_round(1, "网站我去看一下")])).await;
    assert!(gw.state().telegram.enabled());

    // 别人的私聊：不理，也不回话。
    bot.push(1, 99, "你好");
    bot.push(2, OPERATOR, "网页打不开了");
    let (chat, _) = bot
        .wait_sent(|(_, text)| text.contains("网站我去看一下"))
        .await;
    assert_eq!(chat, OPERATOR);
    assert!(bot.sent().iter().all(|(chat, _)| *chat == OPERATOR));

    // 同一个 update 重投（重启后会这样）：不再受理，也不再回一遍。
    bot.push(2, OPERATOR, "网页打不开了");
    bot.push(3, OPERATOR, "/status");
    bot.wait_sent(|(_, text)| text.contains("Komo Gateway 在跑"))
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let replies = bot
        .sent()
        .iter()
        .filter(|(_, text)| text.contains("网站我去看一下"))
        .count();
    assert_eq!(replies, 1, "{:?}", bot.sent());

    // 进的是主对话：网页上能看到这句话。
    let (code, home) = gw.get("/v1/home-session").await;
    assert_eq!(code, 200, "{home}");
    let session: Value = serde_json::from_str(&home).unwrap();
    let (code, tree) = gw
        .get(&format!(
            "/v1/sessions/{}/tree",
            session["session"].as_str().unwrap()
        ))
        .await;
    assert_eq!(code, 200, "{tree}");
    assert!(tree.contains("网页打不开了"), "{tree}");
    gw.stop().await;
}

#[tokio::test]
async fn commands_answer_without_touching_the_conversation() {
    let (_home, gw, bot) = start(FakeLlm::finisher("不该被调用")).await;
    bot.push(1, OPERATOR, "/pending");
    bot.wait_sent(|(_, text)| text.contains("没有在等你答复的"))
        .await;
    bot.push(2, OPERATOR, "/approve");
    bot.wait_sent(|(_, text)| text.contains("没有能 /approve 的"))
        .await;
    bot.push(3, OPERATOR, "/start");
    bot.wait_sent(|(_, text)| text.contains("直接发文字就是在主对话里说话"))
        .await;
    gw.stop().await;
}

#[tokio::test]
async fn what_the_browser_is_told_also_reaches_telegram_in_full() {
    let (_home, gw, bot) = start(FakeLlm::finisher("好")).await;
    let body = "定时任务跑完了：".to_owned() + &"很长的结果。".repeat(40);
    gw.state().notify_operator(PushMessage {
        title: "Komo 的定时任务".into(),
        body: body.clone(),
        url: "/".into(),
        tag: None,
    });
    let (chat, text) = bot
        .wait_sent(|(_, text)| text.contains("Komo 的定时任务"))
        .await;
    assert_eq!(chat, OPERATOR);
    // 浏览器推送截到 140 字；Telegram 这份是全文。
    assert!(text.contains(&body), "{text}");
    gw.stop().await;
}
