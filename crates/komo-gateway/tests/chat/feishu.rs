//! 飞书（Durable 存储，D5 W3a）：与 `../feishu.rs` 同名同断言，Gateway 跑在新存储上。读会话 / 输入用
//! `durable_*` 帮手；其余照旧。原文件的模块文档（入站没走 `FeishuChannel::serve` 的原因）同样适用。
//!
//! 真 `FeishuSender` 对着 loopback 上的假开放平台，真 Dispatcher 在后面。
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

use komo_channel::feishu::{FeishuChannel, FeishuSender, inbound as feishu_inbound};
use komo_gateway::channels::ChannelSender;
use komo_gateway::service::test_support::Gate;
use komo_kernel::protocol::{InboundAck, InboundMessage};
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform};

use crate::harness::{
    FakeLlm, FixedFactory, GatewayBuilder, TestGateway, call_round, config_toml, eventually,
    feishu_block, text_round,
};
use komo_channel::feishu::fake::{
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
        .fake_llm(FakeLlm::finisher("好"))
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

/// 主会话（私聊的线）上展示出来的消息条数：私聊的消息落在这里由分发任务判断去向，不起 Run。
async fn shown(gateway: &TestGateway, session: &str) -> usize {
    gateway
        .state()
        .core()
        .unwrap()
        .read_tree(session)
        .await
        .expect("读得出树")
        .branches
        .iter()
        .flat_map(|branch| branch.entries.iter())
        .filter(|entry| entry.kind == "main.user")
        .count()
}

/// 验证列①：同一 `event_id` 重推只产生一条主线消息、一次分发。
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

    assert_eq!(
        shown(&wired.gateway, session.as_str()).await,
        1,
        "主线上只有一条"
    );
}

/// 验证列②：ws 断线重连不丢事件，也不重复分发。
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

    assert_eq!(
        shown(&wired.gateway, session.as_str()).await,
        2,
        "一共两条消息"
    );
}

/// 飞书私聊里每条开出新话题的主线消息都进主会话的分发（`docs/tasks.md` §4.7）：不开普通会话，两条都落在
/// home 的线上；回话在各自的话题里。
#[tokio::test]
async fn a_feishu_dm_main_message_is_dispatched_from_home_not_opened_as_a_session() {
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
    let (home, _) = wired
        .gateway
        .state()
        .core()
        .unwrap()
        .main_session("assistant")
        .await
        .unwrap();
    assert_eq!(a.as_str(), home.id);
    assert_eq!(b.as_str(), home.id);
    assert_eq!(shown(&wired.gateway, &home.id).await, 2);

    let origins: Vec<String> = wired.gateway.origins().await;
    assert!(
        !origins.iter().any(|origin| origin.starts_with("chat:")),
        "不再按话题开普通会话：{origins:?}"
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
    assert_eq!(ack, InboundAck::Ignored, "群里的消息进分发，回执不触发卡片");

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
    assert_eq!(ack, InboundAck::Ignored);

    // 每个群各有一条线。
    let origins: Vec<String> = wired.gateway.origins().await;
    assert!(
        origins.contains(&"line:feishu:oc_group".to_string()),
        "{origins:?}"
    );
    assert!(
        origins.contains(&"line:feishu:oc_other".to_string()),
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

    assert!(wired.gateway.origins().await.is_empty(), "不建会话");
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

/// 旧办法开出来的话题会话被操作者删掉之后，同一个话题里再 @ 走群的线（由分发判断），不是被拒的旧墓碑。
#[tokio::test]
async fn a_deleted_legacy_thread_session_is_replaced_by_the_groups_line() {
    let wired = wire().await;
    let (_, first) = group_mention("D1", "@_user_1 先说一句", "", "");
    let core = wired.gateway.state().core().unwrap().clone();
    let (old, _) = core
        .session_for_source(&format!("chat:{}", first.peer), "assistant")
        .await
        .unwrap();
    // 话题里还有旧会话：消息仍进它自己。
    let (session, _) = queued(&wired.gateway, first.clone()).await;
    assert_eq!(session.as_str(), old.id);

    // 生命周期（W4a）还没有 Durable 版的 delete：直接把 sessions 行标成已删除。
    wired.gateway.soft_delete(&old.id).await;

    let (_, again) = group_mention("D2", "@_user_1 还在吗", "om_D1", "omt_d");
    assert_eq!(again.peer, first.peer, "还是同一个话题");
    assert_eq!(wired.gateway.handle(again).await, InboundAck::Ignored);
    let origins: Vec<String> = wired.gateway.origins().await;
    assert!(
        origins.contains(&"line:feishu:oc_group".to_string()),
        "删掉的会话不再接收消息：{origins:?}"
    );
}

/// 群里 @ 一次由分发判断：派出的任务绑定这个话题；回执 `Ignored`，渠道不发"处理中"卡片；回复以话题回复发到
/// 话题根；话题里再 @ 直通给同一个任务（不再调 Router）；主流里另起一条 @ 是另一个话题、另一个任务。
#[tokio::test]
async fn a_group_mention_is_answered_in_its_thread_without_a_card() {
    crate::harness::install_crypto();
    let fake = Arc::new(FakeOpenApi::start(Behavior::default()).await);
    let sender = Arc::new(FeishuSender::with_api(fake.api()));
    let gate = Gate::new();
    let router = FakeLlm::finisher(
        &serde_json::json!({ "route": "new", "title": "看日志", "description": "看一下日志" })
            .to_string(),
    );
    let worker = FakeLlm::new(vec![
        vec![
            call_round(1, "pc-1", "gate", serde_json::json!({})),
            text_round(2, "甲好了。\n状态：完成"),
        ],
        vec![text_round(1, "乙好了。\n状态：完成")],
        vec![text_round(1, "丙好了。\n状态：完成")],
    ]);
    let gateway = GatewayBuilder::new(&config_toml(&feishu_block("\"ou_op\"")))
        .env("KOMO_LLM_API_KEY=test-key\nFEISHU_APP_ID=cli_test\nFEISHU_APP_SECRET=test-app-secret\n")
        .llm(router.clone())
        .stream(worker.clone())
        .tool(gate.tool())
        .factory(FixedFactory::sender_only(
            Arc::clone(&sender) as Arc<dyn ChannelSender>
        ))
        .start()
        .await;

    // (a) 主流里 @：话题根就是这条消息；回执是 `Ignored`，渠道不加表情、不发卡片。
    let (_, first) = group_mention("A", "@_user_1 看一下日志", "", "");
    assert_eq!(first.peer.to_string(), "feishu:oc_group#om_A");
    assert_eq!(gateway.handle(first.clone()).await, InboundAck::Ignored);
    gate.wait_entered(1).await;
    assert!(
        fake.calls_to("reply").is_empty(),
        "任务还在做，没有处理中卡片"
    );
    gate.release();
    {
        let fake = Arc::clone(&fake);
        eventually("结果以话题回复发出", move || {
            fake.calls_to("reply").len() == 1
        })
        .await;
    }
    let replies = fake.calls_to("reply");
    assert_eq!(replies[0]["target"], "om_A");
    assert_eq!(replies[0]["reply_in_thread"], serde_json::json!(true));
    assert!(replies[0]["content"].as_str().unwrap().contains("甲好了。"));
    assert!(fake.calls_to("messages").is_empty(), "不能发到群主流");
    assert!(fake.patched_message_ids().is_empty(), "没有卡片可换");

    // (b) 话题里再 @：直通同一个任务，不调 Router。
    let (_, second) = group_mention("B", "@_user_1 再看一眼", "om_A", "omt_x");
    assert_eq!(second.peer, first.peer);
    assert_eq!(gateway.handle(second).await, InboundAck::Ignored);
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
    assert_eq!(router.turns(), 1, "话题里的后续消息直通，没有再判断");

    // (c) 主流里另起一条 @：另一个话题、另一个任务。
    let (_, third) = group_mention("C", "@_user_1 另一件事", "", "");
    assert_eq!(gateway.handle(third).await, InboundAck::Ignored);
    {
        let fake = Arc::clone(&fake);
        eventually("另一个话题收到结果", move || {
            fake.calls_to("reply").len() == 3
        })
        .await;
    }
    assert_eq!(fake.calls_to("reply")[2]["target"], "om_C");
    assert_eq!(router.turns(), 2);

    // 群只有一条线；不再按话题开普通会话。
    assert_eq!(
        gateway.origins().await,
        vec!["line:feishu:oc_group".to_string()]
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
        .fake_llm(FakeLlm::finisher("好"))
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
    use komo_channel::feishu::fake::APP_TENANT;
    let wired = wire_tenant(true, &[]).await;

    let ack = wired
        .say("T1", "oc_group", "ou_colleague", APP_TENANT, "看一下日志")
        .await;
    assert_eq!(ack, InboundAck::Ignored, "同事的群消息进分发：{ack:?}");
    let (record, created) = wired
        .gateway
        .state()
        .core()
        .unwrap()
        .group_line(
            "assistant",
            &ChannelPeer::new(ChannelPlatform::Feishu, "oc_group"),
        )
        .await
        .expect("群的线在");
    assert!(!created, "消息已经建出了这条线");
    assert_eq!(record.kind, komo_kernel::durable::SessionKind::Main);
    assert_eq!(record.agent.as_deref(), Some("assistant"));
    assert_eq!(record.source.as_deref(), Some("line:feishu:oc_group"));

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
    assert!(wired.gateway.origins().await.is_empty(), "不留会话");
}

#[tokio::test]
async fn a_colleague_in_an_external_group_is_rejected_even_with_allow_tenant() {
    use komo_channel::feishu::fake::APP_TENANT;
    let wired = wire_tenant(true, &["oc_ext"]).await;
    let ack = wired
        .say("T4", "oc_ext", "ou_colleague", APP_TENANT, "你好")
        .await;
    assert!(matches!(ack, InboundAck::Rejected { .. }), "{ack:?}");
    assert!(wired.gateway.origins().await.is_empty(), "不留会话");
}

#[tokio::test]
async fn a_colleague_is_rejected_when_allow_tenant_is_off() {
    use komo_channel::feishu::fake::APP_TENANT;
    let wired = wire_tenant(false, &[]).await;
    let ack = wired
        .say("T5", "oc_group", "ou_colleague", APP_TENANT, "你好")
        .await;
    assert!(matches!(ack, InboundAck::Rejected { .. }), "{ack:?}");
}
