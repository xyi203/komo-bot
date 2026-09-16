//! 跨平台：准入、热重载、投递目标、重启补发、会话归属（§11.2、§11.4、§3）。

use std::sync::Arc;

use komo_gateway::channels::ChannelSender;
use komo_kernel::protocol::InboundAck;
use komo_kernel::traits::Notifier;
use komo_kernel::types::chat::{
    ApprovalScope, ChannelPeer, ChannelPlatform, DeliveryState, DeliveryTarget, Outbound, PeerId,
};

use crate::harness::{
    FixedFactory, GatewayBuilder, MemSender, TestGateway, config_toml, feishu_block, inbound,
    telegram_block, telegram_config,
};

fn operator_dm(text: &str, key: &str) -> komo_kernel::protocol::InboundMessage {
    inbound(ChannelPlatform::Telegram, "111", "111", text, key, true)
}

/// 两个渠道都接上（发送口是内存替身，入站由测试直接驱动 Dispatcher）。
async fn both_channels() -> (TestGateway, Arc<MemSender>, Arc<MemSender>) {
    let telegram = MemSender::new(ChannelPlatform::Telegram);
    let feishu = MemSender::new(ChannelPlatform::Feishu);
    let config = config_toml(&format!(
        "{}{}",
        feishu_block("\"ou_op\""),
        telegram_block("111")
    ));
    let gateway = GatewayBuilder::new(&config)
        .env("KOMO_LLM_API_KEY=test-key\nTELEGRAM_BOT_TOKEN=t\nFEISHU_APP_ID=a\nFEISHU_APP_SECRET=b\n")
        .factory(FixedFactory::sender_only(
            Arc::clone(&telegram) as Arc<dyn ChannelSender>
        ))
        .factory(FixedFactory::sender_only(
            Arc::clone(&feishu) as Arc<dyn ChannelSender>
        ))
        .start()
        .await;
    (gateway, telegram, feishu)
}

/// 验证列：不在 `allow_from` 的发送者被拒且**不留记录**，`/id` 对他仍可用。
#[tokio::test]
async fn a_stranger_is_refused_and_leaves_no_trace() {
    let (gateway, _sender) = TestGateway::start().await;

    let ack = gateway
        .handle(inbound(
            ChannelPlatform::Telegram,
            "999",
            "999",
            "在吗",
            "telegram:1",
            true,
        ))
        .await;
    let InboundAck::Rejected { hint } = ack else {
        panic!("不在名单里的应该被拒：{ack:?}");
    };
    // 提示里带着他在这个平台的 id，操作者抄进 allow_from 即可。
    assert!(hint.contains("999"), "{hint}");
    assert!(hint.contains("allow_from"), "{hint}");

    // `/id` 对任何人可用——唯一不要求操作者身份的命令。
    let id = gateway
        .handle(inbound(
            ChannelPlatform::Telegram,
            "999",
            "999",
            "/id",
            "telegram:2",
            true,
        ))
        .await;
    assert!(
        matches!(&id, InboundAck::Replied { text } if text.contains("telegram:999")),
        "{id:?}"
    );

    // 不留任何记录：没有会话、没有 Run、没有投递。
    assert!(
        gateway.sessions().await.is_empty(),
        "被拒绝的消息不该建出会话"
    );
    assert!(
        gateway.pending_deliveries().await.is_empty(),
        "也不该留下投递记录"
    );
}

/// 验证列：把发送者加进 `allow_from` 并保存后，**不重启** Gateway，其下一条消息即进 Run。
#[tokio::test]
async fn a_reloaded_allow_list_decides_the_next_message() {
    let (gateway, _sender) = TestGateway::start().await;
    let newcomer = |key: &str| inbound(ChannelPlatform::Telegram, "222", "222", "在吗", key, true);

    let before = gateway.handle(newcomer("telegram:1")).await;
    assert!(matches!(before, InboundAck::Rejected { .. }), "{before:?}");

    gateway.write_config(&telegram_config("111, 222"));
    gateway.reload().await;

    let after = gateway.handle(newcomer("telegram:2")).await;
    assert!(
        matches!(after, InboundAck::Queued { .. }),
        "改完名单下一条消息就该进 Run：{after:?}"
    );
}

/// §3 第 1 步：校验不过的配置**永远不会被装上**，旧名单继续生效。
#[tokio::test]
async fn an_invalid_reload_keeps_the_old_allow_list() {
    let (gateway, sender) = TestGateway::start().await;

    gateway.write_config(&config_toml(
        r#"
[gateway]
listen = "这不是一个地址"

[channels.telegram]
enabled = true
allow_from = [222]
home_chat = 111
"#,
    ));
    let error = komo_gateway::reload::reload(gateway.state())
        .await
        .expect_err("装不上");
    assert_eq!(
        error.code(),
        komo_kernel::protocol::http::ErrorCode::ConfigInvalid
    );

    // 旧名单仍然生效：111 是操作者，222 还不是。
    assert!(
        gateway
            .state()
            .snapshot()
            .channels
            .telegram
            .is_operator(&PeerId::new("111"))
    );
    let refused = gateway
        .handle(inbound(
            ChannelPlatform::Telegram,
            "222",
            "222",
            "在吗",
            "telegram:9",
            true,
        ))
        .await;
    assert!(
        matches!(refused, InboundAck::Rejected { .. }),
        "{refused:?}"
    );

    // 错误进了 home chat，不是只写日志（§3 第 1 步）。
    assert!(
        sender
            .texts()
            .iter()
            .any(|text| text.contains("配置没装上")),
        "{:?}",
        sender.texts()
    );
}

/// 验证列：审批请求投到**来源会话 + home chat**；第二个答复得到「已决定」。
#[tokio::test]
async fn an_approval_reaches_both_the_source_and_home() {
    let (gateway, sender) = TestGateway::start().await;
    let record = gateway.pending_approval().await;

    // 来源会话是群 222，home chat 是 111。
    let source = ChannelPeer::new(ChannelPlatform::Telegram, "222");
    gateway
        .state()
        .notifier
        .deliver_approval(
            Some(&source),
            komo_runtime::approvals::presentation(&record),
        )
        .await
        .expect("投得出去");

    let targets: Vec<String> = sender
        .sent()
        .iter()
        .filter(|message| matches!(message.outbound, Outbound::ApprovalRequest(_)))
        .map(|message| message.peer.chat_id.to_string())
        .collect();
    assert!(
        targets.contains(&"222".to_string()),
        "来源会话：{targets:?}"
    );
    assert!(
        targets.contains(&"111".to_string()),
        "home chat：{targets:?}"
    );

    // 第一个答复。
    let first = gateway
        .state()
        .decide_approval(&record.approval, true, ApprovalScope::Once, None)
        .await
        .expect("决定得了");
    assert!(!first.already_decided);

    // 第二个答复得到「已决定」，且不改写结论。
    let second = gateway
        .state()
        .decide_approval(&record.approval, false, ApprovalScope::Once, None)
        .await
        .expect("决定得了");
    assert!(second.already_decided, "第二个答复应当得到原决定");
    assert!(second.decision.approved, "结论没有被第二次答复改写");
}

/// §11.4：一个 `home_chat` 都没配时 `deliver_home` **返回错误给调用方，不静默丢弃**。
#[tokio::test]
async fn no_home_chat_is_an_error_not_silence() {
    let gateway = GatewayBuilder::new(&config_toml(
        r#"
[channels.telegram]
enabled = true
allow_from = [111]
"#,
    ))
    .start()
    .await;

    let error = gateway
        .state()
        .notifier
        .deliver_home(Outbound::Text {
            text: "要批一下".into(),
        })
        .await
        .expect_err("没有目标要报错");
    assert!(
        matches!(error, komo_kernel::traits::DeliverError::NoTarget(_)),
        "{error:?}"
    );
}

/// 验证列：Gateway 重启后 pending 投递**补发一次**。
///
// BUG(3): 补发不会发生。`service::start`（`src/service/mod.rs:221`）在
// **第 7 步之后、第 9 步之前**调 `state.notifier.flush(None)`，而渠道是在第 9 步
// （`supervisor.start_all`）才登记发送口的。于是这一次冲刷在
// `DeliveryLog::send_recorded`（`src/deliveries.rs:66`）里走的是"渠道此刻不在"那条路，
// 把每一行原样再标成 `Deferred` 就返回——启动时的补发是个空动作。
// 后果：飞书 / Telegram 这两个**能主动推送**的渠道，重启前没送到的审批请求要等到操作者
// 自己再说一句话（`Dispatcher::handle` 第 4 步的 `flush(Some(peer))`）才会送出去；
// 而§11.4 的整条设计就是"不能为补发一条结果消息重跑任务"。
// 建议：把 `flush(None)` 挪到 `supervisor.start_all` 之后（它不依赖 HTTP 监听，只依赖
// 渠道注册表），或者在 `ChannelRegistry::register` 之后按平台冲刷一次。
#[tokio::test]
async fn a_pending_delivery_is_resent_once_after_a_restart() {
    let first = MemSender::new(ChannelPlatform::Telegram);
    first.defer(true);
    let mut gateway = GatewayBuilder::new(&telegram_config("111"))
        .factory(FixedFactory::sender_only(
            Arc::clone(&first) as Arc<dyn ChannelSender>
        ))
        .start()
        .await;

    let delivery = gateway
        .state()
        .notifier
        .deliver(
            &DeliveryTarget::home(ChannelPeer::new(ChannelPlatform::Telegram, "111")),
            Outbound::Text {
                text: "要批一下".into(),
            },
        )
        .await
        .expect("登记得上");
    assert_eq!(delivery.state, DeliveryState::Deferred);
    assert_eq!(gateway.pending_deliveries().await.len(), 1);

    // 重启：同一个数据目录，一个能推出去的新发送口。
    let second = MemSender::new(ChannelPlatform::Telegram);
    gateway
        .restart_with(
            &telegram_config("111"),
            vec![FixedFactory::sender_only(
                Arc::clone(&second) as Arc<dyn ChannelSender>
            )],
        )
        .await;

    assert_eq!(
        second.texts(),
        vec!["要批一下".to_string()],
        "重启后 pending 投递要补发**一次**"
    );
    assert!(
        gateway.pending_deliveries().await.is_empty(),
        "补发之后不再 pending"
    );
}

/// §11.2：操作者在 Telegram 私聊与飞书私聊说话，落到**同一个 home session**。
#[tokio::test]
async fn every_private_chat_lands_in_one_home_session() {
    let (gateway, _telegram, _feishu) = both_channels().await;

    let first = gateway.handle(operator_dm("早", "telegram:1")).await;
    let InboundAck::Queued { session: home, .. } = first else {
        panic!("{first:?}");
    };

    let second = gateway
        .handle(inbound(
            ChannelPlatform::Feishu,
            "oc_dm",
            "ou_op",
            "接着说",
            "feishu:evt-1",
            true,
        ))
        .await;
    let InboundAck::Queued { session, .. } = second else {
        panic!("{second:?}");
    };
    assert_eq!(session, home, "早上在飞书说的话，回到 Telegram 接着说");

    let origins: Vec<String> = gateway
        .sessions()
        .await
        .into_iter()
        .map(|record| record.origin)
        .collect();
    assert_eq!(
        origins,
        vec!["home".to_string()],
        "只有一个会话：{origins:?}"
    );
}

/// §11.2：群聊按 `{platform}:{chat_id}` 各自一个 Session，且都不是 home。
#[tokio::test]
async fn every_group_gets_its_own_session() {
    let (gateway, _telegram, _feishu) = both_channels().await;

    let telegram_group = gateway
        .handle(inbound(
            ChannelPlatform::Telegram,
            "222",
            "111",
            "看一下日志",
            "telegram:1",
            false,
        ))
        .await;
    let feishu_group = gateway
        .handle(inbound(
            ChannelPlatform::Feishu,
            "oc_group",
            "ou_op",
            "看一下日志",
            "feishu:evt-1",
            false,
        ))
        .await;
    let (InboundAck::Queued { session: a, .. }, InboundAck::Queued { session: b, .. }) =
        (telegram_group, feishu_group)
    else {
        panic!("两个群都该进 Run");
    };
    assert_ne!(a, b, "两个群是两个 Session");

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
            "chat:feishu:oc_group".to_string(),
            "chat:telegram:222".to_string()
        ]
    );

    // 同一个群的第二条消息落回同一个 Session。
    let again = gateway
        .handle(inbound(
            ChannelPlatform::Telegram,
            "222",
            "111",
            "再看一下",
            "telegram:2",
            false,
        ))
        .await;
    assert!(
        matches!(&again, InboundAck::Queued { session, .. } if session == &a),
        "{again:?}"
    );
}

/// §11.3：`/new` 在当前会话上追加一条边界，**不切 Session**。
#[tokio::test]
async fn slash_new_appends_a_boundary_without_switching_sessions() {
    let (gateway, _sender) = TestGateway::start().await;
    let first = gateway.handle(operator_dm("在", "telegram:1")).await;
    let InboundAck::Queued { session, .. } = first else {
        panic!("{first:?}");
    };

    let ack = gateway.handle(operator_dm("/new", "telegram:2")).await;
    let InboundAck::Replied { text } = ack else {
        panic!("{ack:?}");
    };
    assert!(text.contains("新的一段"), "{text}");

    assert_eq!(
        gateway.state().home_session().await.unwrap(),
        session,
        "还是同一个 home session"
    );
    assert_eq!(gateway.sessions().await.len(), 1);

    // 事件流上有那条边界。
    let (status, body) = gateway
        .get(&format!("/v1/sessions/{session}/events?from=0"))
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("conversation.boundary"), "{body}");
}

/// §11.3 命令表：`/status` / `/pending` / `/cancel` 都有回执，且都只对操作者生效。
#[tokio::test]
async fn the_command_table_is_operator_only() {
    let (gateway, _sender) = TestGateway::start().await;
    gateway.pending_approval().await;

    let pending = gateway.handle(operator_dm("/pending", "telegram:1")).await;
    assert!(
        matches!(&pending, InboundAck::Replied { text } if text.contains("shell")),
        "{pending:?}"
    );

    let status = gateway.handle(operator_dm("/status", "telegram:2")).await;
    assert!(
        matches!(&status, InboundAck::Replied { text } if text.contains("待处理审批：1 条")),
        "{status:?}"
    );

    let cancel = gateway.handle(operator_dm("/cancel", "telegram:3")).await;
    assert!(
        matches!(&cancel, InboundAck::Replied { text } if text.contains("没有在跑的任务")),
        "{cancel:?}"
    );

    // 同样的命令，陌生人只得到那条固定提示。
    let stranger = gateway
        .handle(inbound(
            ChannelPlatform::Telegram,
            "999",
            "999",
            "/pending",
            "telegram:4",
            true,
        ))
        .await;
    assert!(
        matches!(stranger, InboundAck::Rejected { .. }),
        "审批命令只接受操作者：{stranger:?}"
    );
}

/// §11.2：`allow_from` 为空的渠道"只出不进"——还能作 home chat 收投递，但没人能下指令。
#[tokio::test]
async fn an_empty_allow_list_still_receives_but_nobody_can_speak() {
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gateway = GatewayBuilder::new(&telegram_config(""))
        .factory(FixedFactory::sender_only(
            Arc::clone(&sender) as Arc<dyn ChannelSender>
        ))
        .start()
        .await;

    let ack = gateway.handle(operator_dm("在", "telegram:1")).await;
    assert!(matches!(ack, InboundAck::Rejected { .. }), "{ack:?}");

    gateway
        .state()
        .notifier
        .deliver_home(Outbound::Text {
            text: "夜里跑完了".into(),
        })
        .await
        .expect("home chat 仍然收得到");
    assert_eq!(sender.texts(), vec!["夜里跑完了".to_string()]);
}
