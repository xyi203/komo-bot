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
use komo_gateway::channels::feishu::{FeishuSender, inbound as feishu_inbound};
use komo_kernel::protocol::{InboundAck, InboundMessage};
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform};

use crate::harness::{FixedFactory, GatewayBuilder, TestGateway, config_toml, feishu_block};
use komo_gateway::channels::feishu::fake::{BOT_OPEN_ID, Behavior, FakeOpenApi, text_event};

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
    let during = text_event("evt-b", "oc_dm", "p2p", "ou_op", "第二件事", &[]);

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

/// §11.2：群里只响应 `groups` 列出的群，且只响应 @机器人 的消息。
#[tokio::test]
async fn a_feishu_group_needs_the_list_and_the_mention() {
    let wired = wire().await;

    // 名单内的群，但没有 @机器人 → 渠道那一步就 None（不进 Dispatcher）。
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

    // @ 了，且群在名单里 → 进 Run。
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

    // @ 了但群不在名单里 → Ignored。
    let outside = text_event(
        "evt-g3",
        "oc_other",
        "group",
        "ou_op",
        "@_user_1 在吗",
        &[("@_user_1", BOT_OPEN_ID)],
    );
    let ack = wired.gateway.handle(decode(&outside, false)).await;
    assert_eq!(ack, InboundAck::Ignored, "只有 groups 列出的群会被响应");

    let origins: Vec<String> = wired
        .gateway
        .sessions()
        .await
        .into_iter()
        .map(|record| record.origin)
        .collect();
    assert!(
        origins.contains(&"chat:feishu:oc_group".to_string()),
        "{origins:?}"
    );
    assert!(
        !origins.contains(&"chat:feishu:oc_other".to_string()),
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
