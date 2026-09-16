//! 飞书：真 `FeishuSender` 对着 loopback 上的假开放平台，真 Dispatcher 在后面。
//!
//! **入站这一半没有走 `FeishuChannel::serve`**，而是把假开放平台给出的原始事件负载喂给
//! `feishu::inbound::parse` + `from_message` / `from_card_action`（渠道自己在
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
use komo_kernel::types::chat::{ApprovalScope, ChannelPeer, ChannelPlatform};

use crate::harness::{FixedFactory, GatewayBuilder, TestGateway, config_toml, feishu_block};
use komo_gateway::channels::feishu::fake::{
    BOT_OPEN_ID, Behavior, FakeOpenApi, card_action_event, text_event,
};

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
fn decode(payload: &[u8], is_private: bool) -> InboundMessage {
    let parsed = feishu_inbound::parse(payload).expect("认得出这条事件");
    let key = feishu_inbound::request_key(&parsed.event_id);
    match parsed.kind {
        feishu_inbound::EventKind::Message(event) => {
            feishu_inbound::from_message(&event, BOT_OPEN_ID, key)
        }
        feishu_inbound::EventKind::CardAction(event) => {
            feishu_inbound::from_card_action(&event, is_private, key)
        }
    }
    .expect("这条要进 Dispatcher")
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

/// 验证列③：卡片回调 → `/approve`，去重键与普通消息同源。
#[tokio::test]
async fn a_card_callback_is_the_same_command_a_message_would_be() {
    let wired = wire().await;
    let record = wired.gateway.pending_approval().await;

    let payload = card_action_event(
        "evt-card",
        "oc_dm",
        "ou_op",
        "approve",
        record.short_id.as_str(),
    );
    let message = decode(&payload, true);
    assert_eq!(message.text, format!("/approve {}", record.short_id));
    assert_eq!(message.request_key.as_str(), "feishu:evt-card");

    let ack = wired.gateway.handle(message).await;
    let InboundAck::Replied { text } = ack else {
        panic!("命令要有回执：{ack:?}");
    };
    assert!(text.contains("已批准"), "{text}");

    let stored = wired
        .gateway
        .state()
        .approval_repo
        .get(&record.approval)
        .await
        .expect("读得到")
        .expect("有这条");
    assert!(stored.decision.expect("有结论").approved);
}

/// 验证列③的另一半：同一 `event_id` 的卡片回调重推只批准一次。
#[tokio::test]
async fn a_replayed_card_callback_decides_once() {
    let wired = wire().await;
    let record = wired.gateway.pending_approval().await;
    let payload = card_action_event(
        "evt-card2",
        "oc_dm",
        "ou_op",
        "reject",
        record.short_id.as_str(),
    );

    let first = wired.gateway.handle(decode(&payload, true)).await;
    assert!(
        matches!(&first, InboundAck::Replied { text } if text.contains("已拒绝")),
        "{first:?}"
    );
    // 平台重推同一条回调（同一个 event_id）：去重表原样返回上一次的回执。
    let again = wired.gateway.handle(decode(&payload, true)).await;
    assert_eq!(again, first, "重推要返回同一条回执");

    let stored = wired
        .gateway
        .state()
        .approval_repo
        .get(&record.approval)
        .await
        .expect("读得到")
        .expect("有这条");
    assert!(!stored.decision.expect("有结论").approved, "结论没有被改写");
}

/// 验证列④：决定后卡片**原地更新**——`PATCH /open-apis/im/v1/messages/{id}`。
#[tokio::test]
async fn a_decision_patches_the_card() {
    let wired = wire().await;
    let record = wired.gateway.pending_approval().await;

    // 先把卡片真的发出去（这一步才让渠道记住它落在哪条消息上）。
    wired
        .gateway
        .state()
        .notifier
        .deliver_approval(None, komo_runtime::approvals::presentation(&record))
        .await
        .expect("投得出去");
    let sent = wired.fake.calls_to("messages");
    assert_eq!(sent.len(), 1, "home chat 收到一张卡片");
    assert_eq!(sent[0]["msg_type"], "interactive", "审批是交互卡片");
    assert!(wired.fake.patched_message_ids().is_empty());

    // 决定。
    wired
        .gateway
        .state()
        .decide_approval(&record.approval, true, ApprovalScope::Once, None)
        .await
        .expect("决定得了");

    let patched = wired.fake.patched_message_ids();
    assert_eq!(patched.len(), 1, "决定之后 PATCH 一次：{patched:?}");
    assert_eq!(patched[0], "om_1000", "PATCH 的是刚才那条卡片");
    let body = wired.fake.calls_to("patch");
    let content = body[0]["content"].as_str().unwrap_or_default();
    assert!(content.contains("已批准"), "卡片换成了结论版：{content}");
    assert!(
        !content.contains("\"button\""),
        "决定过的卡片不该还长着可点的按钮：{content}"
    );
}

/// §11.2：群里只响应 `groups` 列出的群，且只响应 @机器人 的消息。
#[tokio::test]
async fn a_feishu_group_needs_the_list_and_the_mention() {
    let wired = wire().await;

    // 名单内的群，但没有 @机器人 → 渠道那一步就 None（不进 Dispatcher）。
    let plain = text_event("evt-g1", "oc_group", "group", "ou_op", "大家早", &[]);
    let parsed = feishu_inbound::parse(&plain).expect("认得出");
    let feishu_inbound::EventKind::Message(event) = parsed.kind else {
        panic!("是消息事件");
    };
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
