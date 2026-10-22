//! 飞书：真 `FeishuSender` 对着 loopback 上的假开放平台，真 Dispatcher 在后面。
//!
//! **入站这一半没有走 `FeishuChannel::serve`**，而是把假开放平台给出的原始事件负载喂给
//! `feishu::inbound::parse` + `from_message`（渠道自己在
//! `handle_payload` 里做的就是这两步），再交给真 Dispatcher。原因：`FeishuChannel` 的
//! 事件源接缝（`EventSource::Injected` / `FeishuChannel::injected`）与 `with_api` 都是
//! `#[cfg(test)]` / 私有的，集成测试够不着，而 `FeishuChannel::new` 只会去建真 ws。
//! 详见报告"需要编排者做"。因此这一列覆盖的是 **Dispatcher 侧的 durable 去重**
//! （`feishu:{event_id}`，§8.5 的请求键）；渠道自己那层进程内 `SeenEvents` 由
//! `src/channels/feishu/mod.rs` 的单元测试覆盖。

use std::sync::Arc;

use komo_gateway::channels::ChannelSender;
use komo_gateway::channels::feishu::{FeishuChannel, FeishuSender, inbound as feishu_inbound};
use komo_gateway::service::test_support::Gate;
use komo_kernel::protocol::{InboundAck, InboundMessage};
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform};

use crate::harness::{
    FakeLlm, FixedFactory, GatewayBuilder, TestGateway, call_round, config_toml, eventually,
    feishu_block, text_round,
};
use komo_gateway::channels::feishu::fake::{
    BOT_OPEN_ID, Behavior, FakeOpenApi, text_event, text_event_from_tenant, text_event_replying,
};
use komo_kernel::types::ids::{RunId, SessionId};

struct Wired {
    gateway: TestGateway,
    fake: Arc<FakeOpenApi>,
}

async fn wire() -> Wired {
    crate::harness::install_crypto();
    let fake = Arc::new(FakeOpenApi::start(Behavior::default()).await);
    let sender = Arc::new(FeishuSender::with_api(fake.api())) as Arc<dyn ChannelSender>;
    let gateway = GatewayBuilder::new(&config_toml(&feishu_block("\"ou_op\"")))
        .env("KOMO_LLM_API_KEY=test-key\nFEISHU_APP_ID=cli_test\nFEISHU_APP_SECRET=test-app-secret\n")
        .factory(FixedFactory::sender_only(sender))
        .start()
        .await;
    Wired { gateway, fake }
}

/// 渠道 `handle_payload` 的前两步：认出事件 → 转成 `InboundMessage`。
///
/// `is_private` 由事件自己的 `chat_type` 决定；参数只留着让调用处读得出"这是私聊还是群"。
fn decode(payload: &[u8], _is_private: bool) -> InboundMessage {
    let parsed = feishu_inbound::parse(payload).expect("认得出这条事件");
    let key = feishu_inbound::request_key(&parsed.event_id);
    let feishu_inbound::EventKind::Message(event) = parsed.kind;
    feishu_inbound::from_message(&event, BOT_OPEN_ID, key).expect("这条要进 Dispatcher")
}

/// 验证列①：同一 `event_id` 重推只产生一个 Run。
#[tokio::test]
async fn a_replayed_event_produces_one_run() {
    let wired = wire().await;
    let payload = text_event("evt-7", "oc_dm", "p2p", "ou_op", "帮我看看", &[]);

    let first = wired.gateway.handle(decode(&payload, true)).await;
    let InboundAck::Queued { session, run } = first else {
        panic!("第一条应该排队：{first:?}");
    };

    // 飞书是至少一次投递：同一条事件再推一次。
    let again = wired.gateway.handle(decode(&payload, true)).await;
    assert_eq!(
        again,
        InboundAck::Duplicate {
            run: Some(run.clone())
        },
        "重推不该开第二个 Run"
    );

    let runs = wired.gateway.runs_of(&session).await;
    assert_eq!(runs.len(), 1, "账本上只有一个 Run：{runs:?}");
}

/// 验证列②：ws 断线重连不丢事件，也不重跑 Run。
///
/// 断线重连在飞书那边等价于"这一批事件原样再推一次"（官方：即使成功接收仍会收到重复
/// 消息，ws 重连与 3 秒超时都会重推）。这里模拟的是**重连后整批补推**：断线前送到的
/// 一条命中原 Run，断线时没送到的那一条这次才第一次进来。
#[tokio::test]
async fn a_reconnect_loses_nothing_and_reruns_nothing() {
    let wired = wire().await;
    let before = text_event("evt-a", "oc_dm", "p2p", "ou_op", "第一件事", &[]);
    // 第二条接在第一条开出的话题里，同一个会话。
    let during = text_event_replying(
        "evt-b",
        "oc_dm",
        "p2p",
        "ou_op",
        "第二件事",
        &[],
        "om_evt-a",
        "omt_a",
    );

    let first = wired.gateway.handle(decode(&before, true)).await;
    let InboundAck::Queued { session, run } = first else {
        panic!("{first:?}");
    };

    // —— 断线 —— 重连之后平台把两条都再推一遍。
    let replayed = wired.gateway.handle(decode(&before, true)).await;
    assert_eq!(
        replayed,
        InboundAck::Duplicate {
            run: Some(run.clone())
        },
        "断线前已经处理过的不重跑"
    );
    let recovered = wired.gateway.handle(decode(&during, true)).await;
    assert!(
        matches!(recovered, InboundAck::Queued { .. }),
        "断线时没送到的那条不能丢：{recovered:?}"
    );

    let runs = wired.gateway.runs_of(&session).await;
    assert_eq!(runs.len(), 2, "一共两条输入、两个 Run：{runs:?}");
}

/// 飞书私聊也是一个话题一个会话：主线里每一条都开新话题、新会话，都不是 home。
#[tokio::test]
async fn a_feishu_dm_opens_a_thread_session_instead_of_home() {
    let wired = wire().await;
    let first = text_event("evt-d1", "oc_dm", "p2p", "ou_op", "第一件事", &[]);
    let second = text_event("evt-d2", "oc_dm", "p2p", "ou_op", "另一件事", &[]);

    let InboundAck::Queued { session: a, .. } = wired.gateway.handle(decode(&first, true)).await
    else {
        panic!("第一条应该排队");
    };
    let InboundAck::Queued { session: b, .. } = wired.gateway.handle(decode(&second, true)).await
    else {
        panic!("第二条应该排队");
    };
    assert_ne!(a, b, "主线里的两条是两个话题");
    let home = wired.gateway.state().default_main_session().await.unwrap();
    assert_ne!(a, home);
    assert_ne!(b, home);

    let origins: Vec<String> = wired
        .gateway
        .sessions()
        .await
        .into_iter()
        .map(|record| record.origin)
        .collect();
    assert!(
        origins.contains(&"chat:feishu:oc_dm#om_evt-d1".to_string()),
        "{origins:?}"
    );
}

/// §11.2：任何群里，操作者 @机器人 就响应，不用事先登记；没 @ 的消息不进 Dispatcher。
#[tokio::test]
async fn a_feishu_group_needs_only_the_mention() {
    let wired = wire().await;

    // 没有 @机器人 → 渠道那一步就 None（不进 Dispatcher）。
    let plain = text_event("evt-g1", "oc_group", "group", "ou_op", "大家早", &[]);
    let parsed = feishu_inbound::parse(&plain).expect("认得出");
    let feishu_inbound::EventKind::Message(event) = parsed.kind;
    assert!(
        feishu_inbound::from_message(
            &event,
            BOT_OPEN_ID,
            feishu_inbound::request_key(&parsed.event_id)
        )
        .is_none(),
        "群里没 @机器人 的消息不进 Dispatcher"
    );

    // @ 了 → 进 Run。
    let mentioned = text_event(
        "evt-g2",
        "oc_group",
        "group",
        "ou_op",
        "@_user_1 看一下日志",
        &[("@_user_1", BOT_OPEN_ID)],
    );
    let ack = wired.gateway.handle(decode(&mentioned, false)).await;
    assert!(matches!(ack, InboundAck::Queued { .. }), "{ack:?}");

    // 另一个从没登记过的群，@ 了照样进 Run。
    let outside = text_event(
        "evt-g3",
        "oc_other",
        "group",
        "ou_op",
        "@_user_1 在吗",
        &[("@_user_1", BOT_OPEN_ID)],
    );
    let ack = wired.gateway.handle(decode(&outside, false)).await;
    assert!(matches!(ack, InboundAck::Queued { .. }), "{ack:?}");

    let origins: Vec<String> = wired
        .gateway
        .sessions()
        .await
        .into_iter()
        .map(|record| record.origin)
        .collect();
    assert!(
        origins.contains(&"chat:feishu:oc_group#om_evt-g2".to_string()),
        "{origins:?}"
    );
    assert!(
        origins.contains(&"chat:feishu:oc_other#om_evt-g3".to_string()),
        "{origins:?}"
    );
}

/// 一条被拒绝的消息不该在假开放平台上留下任何调用——`/id` 的那条提示由渠道自己回，
/// 不经投递账。
#[tokio::test]
async fn a_feishu_stranger_leaves_no_delivery() {
    let wired = wire().await;
    let payload = text_event("evt-x", "oc_dm", "p2p", "ou_someone", "在吗", &[]);

    let ack = wired.gateway.handle(decode(&payload, true)).await;
    let InboundAck::Rejected { hint } = ack else {
        panic!("不在名单里的应该被拒：{ack:?}");
    };
    assert!(hint.contains("ou_someone"), "{hint}");
    assert!(hint.contains("feishu:oc_dm"), "{hint}");

    assert!(wired.gateway.sessions().await.is_empty(), "不建会话");
    assert!(
        wired.gateway.pending_deliveries().await.is_empty(),
        "不写投递"
    );
    assert!(
        wired.fake.calls_to("messages").is_empty(),
        "也没有经投递账发过任何东西"
    );
    let _ = ChannelPeer::new(ChannelPlatform::Feishu, "oc_dm");
}

fn group_mention(
    event_id: &str,
    text: &str,
    root_id: &str,
    thread_id: &str,
) -> (Vec<u8>, InboundMessage) {
    let payload = text_event_replying(
        event_id,
        "oc_group",
        "group",
        "ou_op",
        text,
        &[("@_user_1", BOT_OPEN_ID)],
        root_id,
        thread_id,
    );
    let message = decode(&payload, false);
    (payload, message)
}

async fn queued(gateway: &TestGateway, message: InboundMessage) -> (SessionId, RunId) {
    match gateway.handle(message).await {
        InboundAck::Queued { session, run } => (session, run),
        other => panic!("应该排队：{other:?}"),
    }
}

/// 话题的 Session 被操作者删掉之后，同一个话题里再 @ 开的是新的 Session，不是被拒的旧墓碑。
#[tokio::test]
async fn a_deleted_thread_session_is_replaced_by_a_fresh_one() {
    let wired = wire().await;
    let (_, first) = group_mention("D1", "@_user_1 先说一句", "", "");
    let (old, _) = queued(&wired.gateway, first.clone()).await;

    wired
        .gateway
        .state()
        .delete_session(&old, true)
        .await
        .expect("操作者删会话");

    let (_, again) = group_mention("D2", "@_user_1 还在吗", "om_D1", "omt_d");
    assert_eq!(again.peer, first.peer, "还是同一个话题");
    let (fresh, _) = queued(&wired.gateway, again).await;
    assert_ne!(fresh, old, "删掉的会话不再接收消息");
}

/// 群里 @ 一次开一个话题、一个话题一个 Session；话题里的所有出站都回到话题里。
#[tokio::test]
async fn a_group_mention_opens_a_thread_that_is_its_own_session() {
    crate::harness::install_crypto();
    let fake = Arc::new(FakeOpenApi::start(Behavior::default()).await);
    let sender = Arc::new(FeishuSender::with_api(fake.api()));
    let gate = Gate::new();
    let gateway = GatewayBuilder::new(&config_toml(&feishu_block("\"ou_op\"")))
        .env("KOMO_LLM_API_KEY=test-key\nFEISHU_APP_ID=cli_test\nFEISHU_APP_SECRET=test-app-secret\n")
        .llm(FakeLlm::new(vec![
            vec![
                call_round(1, "pc-1", "gate", serde_json::json!({})),
                text_round(2, "甲好了。"),
            ],
            vec![text_round(1, "乙好了。")],
            vec![text_round(1, "丙好了。")],
        ]))
        .tool(gate.tool())
        .factory(FixedFactory::sender_only(
            Arc::clone(&sender) as Arc<dyn ChannelSender>
        ))
        .start()
        .await;

    // (a) 主流里 @：话题根就是这条消息，处理中卡片以话题回复发出，结果换在这张卡上。
    let (_, first) = group_mention("A", "@_user_1 看一下日志", "", "");
    assert_eq!(first.peer.to_string(), "feishu:oc_group#om_A");
    let (session_a, run_a) = queued(&gateway, first.clone()).await;
    gate.wait_entered(1).await;
    let ticket = sender.expect_run_card(&run_a, &first.peer);
    let card_id = sender
        .reply_run_card(ticket, "om_A", true)
        .await
        .expect("处理中卡片");
    gate.release();
    {
        let fake = Arc::clone(&fake);
        let card_id = card_id.clone();
        eventually("结果换在卡片上", move || {
            fake.patched_message_ids() == vec![card_id.clone()]
        })
        .await;
    }
    let replies = fake.calls_to("reply");
    assert_eq!(replies.len(), 1, "只有那张处理中卡片：{replies:?}");
    assert_eq!(replies[0]["target"], "om_A");
    assert_eq!(replies[0]["reply_in_thread"], serde_json::json!(true));
    assert!(fake.calls_to("messages").is_empty(), "不能发到群主流");

    // (b) 话题里再 @：同一个 Session；没登记卡片，结果以话题回复发到话题根。
    let (_, second) = group_mention("B", "@_user_1 再看一眼", "om_A", "omt_x");
    assert_eq!(second.peer, first.peer);
    let (session_b, _) = queued(&gateway, second).await;
    assert_eq!(session_b, session_a, "话题里的 @ 落在同一个 Session");
    {
        let fake = Arc::clone(&fake);
        eventually("话题里收到结果", move || {
            fake.calls_to("reply").len() == 2
        })
        .await;
    }
    let replies = fake.calls_to("reply");
    assert_eq!(replies[1]["target"], "om_A");
    assert_eq!(replies[1]["reply_in_thread"], serde_json::json!(true));
    assert!(replies[1]["content"].as_str().unwrap().contains("乙好了。"));
    assert!(fake.calls_to("messages").is_empty(), "不能发到群主流");

    // (c) 主流里另起一条 @：另一个话题、另一个 Session。
    let (_, third) = group_mention("C", "@_user_1 另一件事", "", "");
    let (session_c, _) = queued(&gateway, third).await;
    assert_ne!(session_c, session_a, "另一个话题是另一个 Session");

    let mut origins: Vec<String> = gateway
        .sessions()
        .await
        .into_iter()
        .map(|record| record.origin)
        .collect();
    origins.sort();
    assert_eq!(
        origins,
        vec![
            "chat:feishu:oc_group#om_A".to_string(),
            "chat:feishu:oc_group#om_C".to_string()
        ]
    );
}

// ---------------------------------------------------------------- allow_tenant

/// 转给真 Dispatcher，并记下每条消息的回执——`serve` 把回执吞进了回复里。
struct Tap {
    inner: Arc<komo_gateway::Dispatcher>,
    acks: std::sync::Mutex<Vec<InboundAck>>,
}

#[async_trait::async_trait]
impl komo_kernel::traits::Inbound for Tap {
    async fn handle(
        &self,
        msg: InboundMessage,
    ) -> Result<InboundAck, komo_kernel::traits::GatewayError> {
        let ack = self.inner.handle(msg).await?;
        self.acks.lock().unwrap().push(ack.clone());
        Ok(ack)
    }
}

/// 没有 `allow_from`、只靠 `allow_tenant` 的飞书渠道；事件走真的 `serve`（租户比对与
/// 群信息查询都在那里），后面是真 Dispatcher。
struct Tenant {
    gateway: TestGateway,
    tap: Arc<Tap>,
    events: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    shutdown: komo_kernel::traits::Shutdown,
}

impl Drop for Tenant {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn wire_tenant(allow_tenant: bool, external_chats: &[&str]) -> Tenant {
    use komo_kernel::traits::Channel;
    crate::harness::install_crypto();
    let fake = Arc::new(FakeOpenApi::start(Behavior::external_chats(external_chats)).await);
    let sender = Arc::new(FeishuSender::with_api(fake.api())) as Arc<dyn ChannelSender>;
    let block = format!(
        "\n[channels.feishu]\nenabled = true\nallow_from = []\nallow_tenant = {allow_tenant}\n\
         home_chat = \"oc_home\"\n"
    );
    let gateway = GatewayBuilder::new(&config_toml(&block))
        .env("KOMO_LLM_API_KEY=test-key\nFEISHU_APP_ID=cli_test\nFEISHU_APP_SECRET=test-app-secret\n")
        .factory(FixedFactory::sender_only(sender))
        .start()
        .await;
    let tap = Arc::new(Tap {
        inner: Arc::clone(gateway.dispatcher()),
        acks: Default::default(),
    });
    let (events, rx) = tokio::sync::mpsc::unbounded_channel();
    let channel = FeishuChannel::injected(fake.api(), rx);
    let shutdown = komo_kernel::traits::Shutdown::new();
    {
        let (tap, shutdown) = (Arc::clone(&tap), shutdown.clone());
        tokio::spawn(async move {
            let _keep = fake;
            let _ = channel.serve(tap, shutdown).await;
        });
    }
    Tenant {
        gateway,
        tap,
        events,
        shutdown,
    }
}

impl Tenant {
    /// 群里 @机器人，等 Dispatcher 给出回执。
    async fn say(
        &self,
        event: &str,
        chat: &str,
        sender: &str,
        tenant: &str,
        text: &str,
    ) -> InboundAck {
        let before = self.tap.acks.lock().unwrap().len();
        self.events
            .send(text_event_from_tenant(
                event,
                chat,
                "group",
                sender,
                tenant,
                &format!("@_user_1 {text}"),
                &[("@_user_1", BOT_OPEN_ID)],
            ))
            .unwrap();
        let tap = Arc::clone(&self.tap);
        eventually("Dispatcher 给出回执", move || {
            tap.acks.lock().unwrap().len() > before
        })
        .await;
        self.tap.acks.lock().unwrap()[before].clone()
    }
}

#[tokio::test]
async fn a_colleague_in_an_internal_group_is_an_operator_with_allow_tenant() {
    use komo_gateway::channels::feishu::fake::APP_TENANT;
    let wired = wire_tenant(true, &[]).await;

    let ack = wired
        .say("T1", "oc_group", "ou_colleague", APP_TENANT, "看一下日志")
        .await;
    let InboundAck::Queued { session, .. } = ack else {
        panic!("同事应该排队：{ack:?}");
    };
    let record = komo_store::repos::session::get(&wired.gateway.state().db, &session)
        .await
        .unwrap()
        .expect("会话在");
    assert_eq!(record.kind, komo_store::models::SessionKind::Normal);
    assert_eq!(record.agent_id, "assistant");
    assert_eq!(record.origin, "chat:feishu:oc_group#om_T1");

    let status = wired
        .say("T2", "oc_group", "ou_colleague", APP_TENANT, "/status")
        .await;
    assert!(matches!(status, InboundAck::Replied { .. }), "{status:?}");
}

#[tokio::test]
async fn an_external_tenant_sender_is_rejected_even_with_allow_tenant() {
    let wired = wire_tenant(true, &[]).await;
    let ack = wired
        .say("T3", "oc_group", "ou_outsider", "tenant_other", "你好")
        .await;
    assert!(matches!(ack, InboundAck::Rejected { .. }), "{ack:?}");
    assert!(wired.gateway.sessions().await.is_empty(), "不留会话");
}

#[tokio::test]
async fn a_colleague_in_an_external_group_is_rejected_even_with_allow_tenant() {
    use komo_gateway::channels::feishu::fake::APP_TENANT;
    let wired = wire_tenant(true, &["oc_ext"]).await;
    let ack = wired
        .say("T4", "oc_ext", "ou_colleague", APP_TENANT, "你好")
        .await;
    assert!(matches!(ack, InboundAck::Rejected { .. }), "{ack:?}");
    assert!(wired.gateway.sessions().await.is_empty(), "不留会话");
}

#[tokio::test]
async fn a_colleague_is_rejected_when_allow_tenant_is_off() {
    use komo_gateway::channels::feishu::fake::APP_TENANT;
    let wired = wire_tenant(false, &[]).await;
    let ack = wired
        .say("T5", "oc_group", "ou_colleague", APP_TENANT, "你好")
        .await;
    assert!(matches!(ack, InboundAck::Rejected { .. }), "{ack:?}");
}
