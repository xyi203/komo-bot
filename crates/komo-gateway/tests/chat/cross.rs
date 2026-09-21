//! 跨平台：准入、热重载、投递目标、重启补发、会话归属（§11.2、§11.4、§3）。

use std::sync::Arc;

use komo_gateway::channels::ChannelSender;
use komo_kernel::events::EventPayload;
use komo_kernel::protocol::InboundAck;
use komo_kernel::traits::{LlmClient, Notifier};
use komo_kernel::types::chat::{
    ApprovalScope, ChannelPeer, ChannelPlatform, DeliveryState, DeliveryTarget, Outbound, PeerId,
};
use komo_kernel::types::status::{RunState, WaitReason};

use crate::harness::{
    EVENT_DEADLINE, FakeLlm, FixedFactory, GatewayBuilder, Gw, Home, MemSender, TestGateway,
    call_round, config_toml, eventually, feishu_block, inbound, multi_call_round, telegram_block,
    telegram_config, text_round,
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
/// 补发**在就绪之后**（`service::start` 的第 11 步），所以它是"尽快发生"而不是"返回之前
/// 已经发生"——断言要等它，不能假设 `start` 回来时它已经跑完。这个次序是有代价换来的：
/// 它曾经同步跑在 "Gateway 就绪" 之前，于是卡住的投递直接把重启拖长（§11.4）。
///
/// 顺序上有一条硬要求：**渠道登记之后**才有意义。`DeliveryLog::send_recorded` 找不到
/// 发送口就把行原样留在 pending，所以在 `supervisor.start_all` 之前冲刷等于什么都没干
/// （W5 验收 BUG(3)）。
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

    let watching = Arc::clone(&second);
    eventually("重启后 pending 投递补发一次", move || {
        watching.texts() == vec!["要批一下".to_string()]
    })
    .await;
    let left = gateway.pending_deliveries().await;
    assert!(left.is_empty(), "补发之后不再 pending：{left:?}");
}

/// **就绪不等补发**：积压的投递再慢，也只慢在后台（§3、§11.4）。
///
/// 这一条守的是重启耗时。补发是网络 I/O，一条 pending 一个平台往返；它同步跑在
/// "Gateway 就绪" 之前时，积压多少就等多久——线上 10 条卡住的投递是 3 秒，几百条能拖过
/// 客户端 60 秒的就绪超时，而这段时间里客户端连不上、渠道也还没开始收消息。
#[tokio::test]
async fn becoming_ready_does_not_wait_for_the_delivery_backlog() {
    let first = MemSender::new(ChannelPlatform::Telegram);
    first.defer(true);
    let mut gateway = GatewayBuilder::new(&telegram_config("111"))
        .factory(FixedFactory::sender_only(
            Arc::clone(&first) as Arc<dyn ChannelSender>
        ))
        .start()
        .await;

    // 三条卡住的投递：每次补发要跟平台往返三次。
    for text in ["一".to_string(), "二".to_string(), "三".to_string()] {
        gateway
            .state()
            .notifier
            .deliver(
                &DeliveryTarget::home(ChannelPeer::new(ChannelPlatform::Telegram, "111")),
                Outbound::Text { text },
            )
            .await
            .expect("登记得上");
    }
    assert_eq!(gateway.pending_deliveries().await.len(), 3);

    // 新发送口每条要 1 秒：三条就是 3 秒。同步补发的话，重启至少要 3 秒。
    let second = MemSender::new(ChannelPlatform::Telegram);
    second.slow_down(std::time::Duration::from_secs(1));
    let started = std::time::Instant::now();
    gateway
        .restart_with(
            &telegram_config("111"),
            vec![FixedFactory::sender_only(
                Arc::clone(&second) as Arc<dyn ChannelSender>
            )],
        )
        .await;
    let ready = started.elapsed();
    assert!(
        ready < std::time::Duration::from_millis(1500),
        "就绪等了 {ready:?}——补发又回到就绪之前了"
    );

    // 补发照样发生，只是晚一步。等它（3 秒的往返 + 余量）。
    let watching = Arc::clone(&second);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while watching.sent().len() < 3 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(watching.sent().len(), 3, "就绪不等补发，但补发还是要发生");
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
    // §7.5：待处理是三类合起来的一张清单，`/status` 报总数与各类的分布——那条审批算在
    // 「审批」那一格里。
    assert!(
        matches!(
            &status,
            InboundAck::Replied { text } if text.contains("待处理（共 1 条）：审批 1")
        ),
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

/// **一条真的消息**停在等待审批上时，那条审批请求真的被投出去了。
///
/// 这一条与上面那个 `an_approval_reaches_both_the_source_and_home` 的分别是**谁在投**：
/// 那一条直接调 `deliver_approval`（证的是目标怎么挑），这一条什么都不调——一条 Telegram
/// 私聊消息进来、模型要跑 `shell`、Policy 答 Ask，然后审批请求应当自己出现在两个会话里。
/// §7.4 的「投递到 Run 的来源会话与 home chat」说的是这条生产路径，而它一度**只有测试在
/// 走**：TUI 靠轮询 `/v1/approvals` 才看得见，聊天渠道什么都收不到。
#[tokio::test]
async fn a_waiting_run_sends_its_approval_to_the_chat_and_home() {
    let sender = MemSender::new(ChannelPlatform::Telegram);
    // home chat 故意与私聊不是同一个会话，这样"各一条"才说得清。
    let gateway = GatewayBuilder::new(&config_toml(
        r#"
[channels.telegram]
enabled = true
allow_from = [111]
home_chat = 999
"#,
    ))
    .llm(FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-1",
            "shell",
            serde_json::json!({"command": "echo hi"}),
        ),
        text_round(2, "跑完了。"),
    ]]))
    .factory(FixedFactory::sender_only(
        Arc::clone(&sender) as Arc<dyn ChannelSender>
    ))
    .start()
    .await;

    let ack = gateway
        .handle(operator_dm("跑一下 echo", "telegram:1"))
        .await;
    assert!(matches!(ack, InboundAck::Queued { .. }), "{ack:?}");

    let watching = Arc::clone(&sender);
    eventually("两个会话都收到了审批请求", move || {
        watching
            .sent()
            .iter()
            .filter(|message| matches!(message.outbound, Outbound::ApprovalRequest(_)))
            .count()
            >= 2
    })
    .await;

    let mut targets: Vec<String> = sender
        .sent()
        .iter()
        .filter(|message| matches!(message.outbound, Outbound::ApprovalRequest(_)))
        .map(|message| message.peer.chat_id.to_string())
        .collect();
    targets.sort();
    assert_eq!(
        targets,
        vec!["111".to_string(), "999".to_string()],
        "来源会话与 home chat **各一条**，不多不少"
    );
}

/// 停在等待审批时，**会话账本里也有那一条**——界面靠它看得见审批。
///
/// `run.waiting_approval` 只说"停下了"，短 ID 在 `approval.requested` 身上：折叠与 SSE
/// 的待处理帧都是从它派生的。少了它，TUI 停在"等待审批"却没有待审批计数、也不弹窗，
/// 只能自己轮询 `/v1/approvals` 才发现自己在等（§7.4「TUI 同时可见」）。
///
/// 权威在 state.db，JSONL 那一条是**反向补写**的审计副本（§8.5）。补写的时机是这里
/// 测的东西：它曾经只由启动与 `AUDIT_TICK`（60s）各做一次，于是弹窗最多晚一分钟才
/// 出现——现在停在待审批上就按一次叫醒铃，**不手动调补写器**。
#[tokio::test]
async fn a_waiting_run_writes_the_approval_request_into_the_session_log() {
    let home = Home::new();
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-1",
            "shell",
            serde_json::json!({"command": "echo hi"}),
        ),
        text_round(2, "跑完了。"),
    ]]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gw.open_session().await;
    // **先订阅再交任务**：界面看见弹窗靠的就是这一条直播帧，而它由那次补写派生。
    let mut frames = gw.state().hub.subscribe(&session);
    let run = gw.submit(&session, "approval-1", "跑一下 echo").await.run;
    // §8.4：停在审批上是 `state == waiting` 加一个说得出理由的 `wait`——两维合起来才是
    // 旧的那一个 `waiting_approval`。
    let wait = gw.wait_waiting(&run).await;
    assert!(
        matches!(wait, WaitReason::Approval { .. }),
        "停在等待上就该说得出在等审批：{wait:?}"
    );

    let event = home
        .wait_event(&session, "approval.requested", EVENT_DEADLINE)
        .await;
    let EventPayload::ApprovalRequested(requested) = event.payload else {
        panic!("{} 不是审批请求", event.type_name())
    };
    let pending = home.wait_approval_frame(&mut frames, EVENT_DEADLINE).await;
    assert_eq!(pending.approval, requested.approval, "帧与账本说同一条");
    assert_eq!(pending.short_id, requested.short_id);

    // 它是**答得了**的那一条：界面拿这个 ID 去取详情、去答复。
    let pending = gw.approvals().await;
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(requested.approval.as_str(), pending[0].approval.as_str());
    assert_eq!(requested.short_id, pending[0].short_id);
    assert_eq!(requested.plan_hash, pending[0].plan_hash);

    // 再补一次不会多出第二条（按 `event_id` 幂等）。
    assert_eq!(gw.state().drain_audit().await, 0, "没有第二条");
    assert_eq!(
        home.event_types(&session)
            .iter()
            .filter(|name| *name == "approval.requested")
            .count(),
        1
    );
}

/// §8.4：前一个 Run 还停在等待审批时，**同一 Session 的下一条消息不越过它**。
///
/// 越过去不是"顺序不好看"：停住的那半轮里那次调用还没有结果，回放窗口只能带着一个没有
/// 输出的 `function_call` 发给模型，provider 直接 400（`No tool output found for tool
/// call …`）——用户在审批弹窗之外敲的那句 `y` 就是这么变成一次失败的。
#[tokio::test]
async fn a_message_behind_a_waiting_approval_waits_instead_of_overtaking() {
    let home = Home::new();
    let llm = FakeLlm::new(vec![
        // 第一段：要跑 shell → 停在审批。
        vec![call_round(
            1,
            "pc-1",
            "shell",
            serde_json::json!({"command": "echo hi"}),
        )],
        // 批准之后续跑的那一段。
        vec![text_round(2, "跑完了。")],
        // 排队那条 Run 的那一段。
        vec![text_round(1, "y")],
    ]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gw.open_session().await;

    let first = gw.submit(&session, "overtake-1", "跑一下 echo").await.run;
    let wait = gw.wait_waiting(&first).await;
    assert!(
        matches!(wait, WaitReason::Approval { .. }),
        "前一个 Run 停下的理由是等审批：{wait:?}"
    );
    let turns_before = llm.turns();

    // 同一个 Session 再来一条：前一个没结束，它不许越过。§8.4 拆成两维之后，这个形态是
    // `waiting` + `dependency`，而且**说得出在等哪一条 Run**——旧模型里的"排队中"正好是
    // 要消掉的那句"排队二十分钟，不知道为什么"。
    let second = gw.submit(&session, "overtake-2", "y").await.run;
    let blocked = gw.wait_waiting(&second).await;
    assert_eq!(
        blocked,
        WaitReason::Dependency { run: first.clone() },
        "后一条要说出在等哪一条 Run"
    );
    let detail = gw.run_detail(&second).await;
    assert_eq!(
        detail.summary.state,
        RunState::Waiting,
        "前一个还停在等待审批，后一个不许领"
    );
    assert_eq!(
        detail.summary.wait,
        Some(WaitReason::Dependency { run: first.clone() })
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(llm.turns(), turns_before, "排队的这条不该走到模型");
    assert_eq!(gw.run_state(&second).await, RunState::Waiting);

    // 答复之后前一个跑完，后一个才轮到。
    let record = gw
        .approvals()
        .await
        .into_iter()
        .find(|candidate| candidate.run.as_ref() == Some(&first))
        .expect("那条审批");
    gw.decide(&record.approval, true).await;
    gw.wait_terminal(&first).await;
    gw.wait_terminal(&second).await;

    // 而且交给模型的每一份转写都过得了 provider 那一关。
    let requests = llm.requests.lock().expect("脚本模型").clone();
    assert_transcripts_close_every_call(&requests);

    // §8.3：后一条 Run 的窗口里要看得见前一条说了什么、最后答了什么——用户那句"接着上面
    // 说"指的就是它，少了这两句，模型只能靠猜（真实会话里它去读了 `config.toml`）。
    let turns: Vec<&str> = requests
        .last()
        .expect("脚本模型收到了请求")
        .messages
        .iter()
        .filter_map(|message| message.text.as_deref())
        .collect();
    assert!(
        turns.contains(&"跑一下 echo"),
        "上一轮的用户输入：{turns:?}"
    );
    assert!(turns.contains(&"跑完了。"), "上一轮的最后回复：{turns:?}");
    assert!(turns.contains(&"y"), "这一轮的输入：{turns:?}");
}

/// §8.3 / §8.4：一轮要了两次调用、**第一个就停下**时，续跑要把后面的也跑完。
///
/// `tool.planned` 是执行到那个调用才写的，所以第一个停在审批上时，**第二个连计划都还
/// 没有**。按"有计划的调用"恢复，续跑只跑完第一个，模型下一轮拿到的是"要了两次、只回了
/// 一次输出"的转写——provider 直接 400（`No tool output found for tool call …`），这条
/// Run 从此不可能完成。
#[tokio::test]
async fn a_round_stopped_at_its_first_call_still_finishes_the_rest() {
    let home = Home::new();
    let llm = FakeLlm::new(vec![
        vec![multi_call_round(
            1,
            &[
                (
                    "call_00_one",
                    "shell",
                    serde_json::json!({"command": "echo one"}),
                ),
                (
                    "call_01_two",
                    "shell",
                    serde_json::json!({"command": "echo two"}),
                ),
            ],
        )],
        vec![text_round(2, "两条都跑完了。")],
    ]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "two-calls", "跑两条命令").await.run;
    let wait = gw.wait_waiting(&run).await;
    assert!(
        matches!(wait, WaitReason::Approval { .. }),
        "第一次停下就是等审批：{wait:?}"
    );

    // 批准第一个。它跑掉之后，第二个必须也停下来问一次——而不是被跳过。
    let first = pending_for(&gw, &run).await;
    gw.decide(&first.approval, true).await;
    let second = wait_for_another_approval(&gw, &run, &first).await;
    assert_ne!(
        second.call, first.call,
        "第二个调用是另一个调用：它原先连计划都没有"
    );
    gw.decide(&second.approval, true).await;
    gw.wait_terminal(&run).await;

    // 两次调用都真的跑过。
    let results = home
        .events(&session)
        .iter()
        .filter(|event| event.type_name() == "tool.result" && event.run.as_ref() == Some(&run))
        .count();
    assert_eq!(
        results,
        2,
        "一轮里的两次调用都要有结果：{:?}",
        home.event_types(&session)
    );

    // 而且每一步交给模型的转写都过得了 provider 那一关。
    let requests = llm.requests.lock().expect("脚本模型").clone();
    assert_transcripts_close_every_call(&requests);
}

/// 这个 Run 当下的待审批（不操心是第几条）。
async fn pending_for(
    gw: &Gw,
    run: &komo_kernel::types::ids::RunId,
) -> komo_kernel::protocol::http::ApprovalRecord {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(record) = gw
            .approvals()
            .await
            .into_iter()
            .find(|candidate| candidate.run.as_ref() == Some(run))
        {
            return record;
        }
        assert!(std::time::Instant::now() < deadline, "这条 Run 没有待审批");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// 等**下一条**审批出现（同一 Run，调用号与刚答过的那条不同）。
async fn wait_for_another_approval(
    gw: &Gw,
    run: &komo_kernel::types::ids::RunId,
    answered: &komo_kernel::protocol::http::ApprovalRecord,
) -> komo_kernel::protocol::http::ApprovalRecord {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let found = gw.approvals().await.into_iter().find(|candidate| {
            candidate.run.as_ref() == Some(run)
                && candidate.approval != answered.approval
                && candidate.call != answered.call
        });
        if let Some(record) = found {
            return record;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "第一个调用批准之后，第二个调用应当接着问——它被跳过了"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// 转写里**调用还没有输出之前，不能出现下一条用户消息**。
///
/// 这就是 provider 判的那条（`No tool output found for tool call …`）：夹在中间的用户消息
/// 会让那次调用看起来永远没有输出。排队那条 Run 的输入恰好会长在这个位置——接收即落盘
/// （§8.5），而它开跑要等前一个结束。
fn assert_transcripts_close_every_call(requests: &[komo_kernel::types::turn::TurnRequest]) {
    for (index, request) in requests.iter().enumerate() {
        let mut open = 0usize;
        for message in &request.messages {
            let shape = format!(
                "{:?}({}){:?}",
                message.role,
                message.tool_calls.len(),
                message.text.as_deref().unwrap_or("")
            );
            match message.role {
                komo_kernel::types::turn::Role::User => assert_eq!(
                    open,
                    0,
                    "第 {index} 份转写把用户消息夹在了没有输出的调用中间：{shape}；\n{}",
                    transcript(request)
                ),
                komo_kernel::types::turn::Role::Assistant => open += message.tool_calls.len(),
                komo_kernel::types::turn::Role::Tool => open = 0,
            }
        }
    }
}

fn transcript(request: &komo_kernel::types::turn::TurnRequest) -> String {
    request
        .messages
        .iter()
        .map(|message| {
            format!(
                "  {:?} calls={} results={} text={:?}",
                message.role,
                message.tool_calls.len(),
                message.tool_results.len(),
                message.text.as_deref().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// HTTP 提交的 Run 没有来源会话：审批请求**只有 home chat**（§11.4「来源是 Cron 或已
/// 断开的 TUI 时只有 home chat」）。
#[tokio::test]
async fn an_http_run_sends_its_approval_to_home_only() {
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gateway = GatewayBuilder::new(&telegram_config("111"))
        .llm(FakeLlm::new(vec![vec![
            call_round(
                1,
                "pc-1",
                "shell",
                serde_json::json!({"command": "echo hi"}),
            ),
            text_round(2, "跑完了。"),
        ]]))
        .factory(FixedFactory::sender_only(
            Arc::clone(&sender) as Arc<dyn ChannelSender>
        ))
        .start()
        .await;

    let (code, body) = gateway.post("/v1/sessions", serde_json::json!({})).await;
    assert_eq!(code, 200, "{body}");
    let summary: komo_kernel::protocol::http::SessionSummary =
        serde_json::from_str(&body).expect("会话");
    let (code, body) = gateway
        .post(
            &format!("/v1/sessions/{}/runs", summary.session),
            serde_json::json!({ "request_key": "http-1", "text": "跑一下 echo" }),
        )
        .await;
    assert_eq!(code, 200, "{body}");

    let watching = Arc::clone(&sender);
    eventually("home chat 收到了审批请求", move || {
        watching
            .sent()
            .iter()
            .any(|message| matches!(message.outbound, Outbound::ApprovalRequest(_)))
    })
    .await;

    let targets: Vec<String> = sender
        .sent()
        .iter()
        .filter(|message| matches!(message.outbound, Outbound::ApprovalRequest(_)))
        .map(|message| message.peer.chat_id.to_string())
        .collect();
    assert_eq!(
        targets,
        vec!["111".to_string()],
        "没有来源会话，就**只有** home chat 这一条"
    );
}

/// **一次答一批**（§11.3 的 `/approve all`，`POST /v1/interventions/answers`）。
///
/// 两个会话各停一条 shell 审批（不同会话才谈得上"各自卡着"，同一会话里后一个 Run 不许
/// 越过前一个）。一次请求答两条，两条 Run 都接着跑完——**每一条各自落一条决定、各自换
/// 一份凭据**：批量省的是按键，不是把两次授权合并成一次。
#[tokio::test]
async fn one_batch_decision_answers_every_pending_approval() {
    let home = Home::new();
    // 每个 Run 一段：要一次 shell（停在审批），脚本演完之后的那一句收尾由 FakeLlm 的
    // 兜底给——两条 Run 的续跑先后不定，断言不依赖谁先谁后。
    let llm = FakeLlm::new(vec![
        vec![call_round(
            1,
            "pc-1",
            "shell",
            serde_json::json!({"command": "echo one"}),
        )],
        vec![call_round(
            1,
            "pc-2",
            "shell",
            serde_json::json!({"command": "echo two"}),
        )],
    ]);
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    let mut runs = Vec::new();
    let mut handles = Vec::new();
    for index in 0..2 {
        let session = gw.open_session().await;
        let run = gw
            .submit(&session, &format!("batch-{index}"), "跑一下 echo")
            .await
            .run;
        let wait = gw.wait_waiting(&run).await;
        assert!(
            matches!(wait, WaitReason::Approval { .. }),
            "两条 Run 都停在等审批上：{wait:?}"
        );
        let record = pending_for(&gw, &run).await;
        runs.push(run);
        // 批量答复的名单列的是 §7.5 清单上的**句柄**：审批的句柄就是短 ID。
        handles.push(record.short_id.to_string());
    }
    assert_eq!(gw.approvals().await.len(), 2, "两条都在等");

    let (code, body) = gw
        .post(
            "/v1/interventions/answers",
            serde_json::json!({
                "handles": handles,
                "approved": true,
                "request_key": "batch-1",
            }),
        )
        .await;
    assert_eq!(code, 200, "{body}");
    let response: komo_kernel::protocol::http::InterventionBatchAnswerResponse =
        serde_json::from_str(&body).expect("批量回执");
    assert_eq!(response.answered.len(), 2, "{body}");
    assert!(response.missing.is_empty(), "{body}");
    // **每一条各自落一条结论**，而且各自对着自己那个句柄——不是一条结论覆盖两条。
    for (answer, handle) in response.answered.iter().zip(&handles) {
        assert_eq!(&answer.handle, handle, "{body}");
        assert_eq!(
            answer.verdict,
            komo_kernel::protocol::http::InterventionVerdict::Approve,
            "{body}"
        );
        let decision = answer.decision.as_ref().expect("审批的答复带着决定");
        assert!(decision.approved, "{body}");
        assert!(!answer.already_answered, "第一次答复不该是「已经答过」");
    }

    // 两条 Run 都接着跑完。
    for run in &runs {
        gw.wait_terminal(run).await;
    }
    assert!(gw.approvals().await.is_empty(), "没有剩下的待处理审批");

    // 同样的名单再答一次：每一条都返回**它自己那个原决定**，不报错（§11.3）。
    let (code, body) = gw
        .post(
            "/v1/interventions/answers",
            serde_json::json!({
                "handles": handles,
                "approved": true,
                "request_key": "batch-2",
            }),
        )
        .await;
    assert_eq!(code, 200, "{body}");
    let again: komo_kernel::protocol::http::InterventionBatchAnswerResponse =
        serde_json::from_str(&body).expect("批量回执");
    assert_eq!(again.answered.len(), 2, "{body}");
    for (answer, handle) in again.answered.iter().zip(&handles) {
        assert_eq!(&answer.handle, handle, "{body}");
        assert!(answer.already_answered, "第二次答复得到的是原决定：{body}");
    }
}

/// 名单里夹着一条**不存在的**审批：其余照答，不存在的单独列出来（不让整批失败）。
///
/// 一批里夹着一条刚刚在别的界面答掉的请求是常态（手机、另一台机器、另一个 TUI），为它
/// 把其余几条一起挡下，等于逼操作者去猜是哪一条不见了。
#[tokio::test]
async fn a_batch_skips_the_names_that_are_not_there() {
    let (gateway, _sender) = TestGateway::start().await;
    let record = gateway.pending_approval().await;
    // 名单上的每一项都是 §7.5 清单给的**句柄**（审批 = 短 ID），不存在的那个也照写成句柄。
    let handle = record.short_id.to_string();

    let (code, body) = gateway
        .post(
            "/v1/interventions/answers",
            serde_json::json!({
                "handles": [handle.clone(), "ap-nobody".to_string()],
                "approved": true,
            }),
        )
        .await;
    assert_eq!(code, 200, "{body}");
    let response: komo_kernel::protocol::http::InterventionBatchAnswerResponse =
        serde_json::from_str(&body).expect("批量回执");
    assert_eq!(response.answered.len(), 1, "{body}");
    assert_eq!(response.answered[0].handle, handle, "{body}");
    assert_eq!(
        response.missing,
        vec!["ap-nobody".to_string()],
        "不存在的那个单独列出来：{body}"
    );
    assert!(gateway.approvals().await.is_empty(), "那一条真的答掉了");
}

/// 聊天里的 `/approve all`：**一次把待处理的全部答了**（§11.3）。
///
/// 三条待处理时，`/approve`（不带 ID）只回一句"请指明"——那是设计；操作者要的是
/// `/approve all` 这一句，而它必须真的答掉全部，并在回执里**点名**答了哪几条。
#[tokio::test]
async fn the_chat_all_command_answers_every_pending_one() {
    let (gateway, _sender) = TestGateway::start().await;
    for _ in 0..3 {
        gateway.pending_approval().await;
    }
    let short_ids: Vec<String> = gateway
        .approvals()
        .await
        .iter()
        .map(|record| record.short_id.to_string())
        .collect();
    assert_eq!(short_ids.len(), 3, "{short_ids:?}");

    // 不带 ID 的老路：多于一条就列出来要求指明，不猜。
    let ambiguous = gateway.handle(operator_dm("/approve", "telegram:1")).await;
    let InboundAck::Replied { text } = ambiguous else {
        panic!("{ambiguous:?}");
    };
    assert!(text.contains("请指明"), "{text}");
    assert!(text.contains("/approve all"), "要说得出怎么全批：{text}");
    assert_eq!(gateway.approvals().await.len(), 3, "那句不改变任何状态");

    let ack = gateway
        .handle(operator_dm("/approve all", "telegram:2"))
        .await;
    let InboundAck::Replied { text } = ack else {
        panic!("{ack:?}");
    };
    assert!(text.contains("3 条"), "{text}");
    assert!(text.contains("各按本次调用"), "{text}");
    for short_id in &short_ids {
        assert!(text.contains(short_id), "回执要点名答了哪几条：{text}");
    }
    assert!(gateway.approvals().await.is_empty(), "三条都答掉了");

    // 已经答过的再来一次：不报错，也不假装又答了一遍。
    let again = gateway
        .handle(operator_dm("/approve all", "telegram:3"))
        .await;
    assert!(
        matches!(&again, InboundAck::Replied { text } if text.contains("没有待处理")),
        "{again:?}"
    );
}
