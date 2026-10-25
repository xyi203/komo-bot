//! 跨平台：准入、热重载、投递目标、重启补发、会话归属（§11.2、§11.4、§3）——Durable 存储版
//! （D5 W3a）：与 `../cross.rs` 同名同断言，Gateway 跑在新存储上。
//!
//! 与 Legacy 不同之处各在对应测试的注释里写了理由。没有迁过来的四个（都建立在旧的 `waiting +
//! intervention` 之上，新存储里 intervention 只有 faulted 任务一种，投递与清单由 W4a 接）：
//! `an_intervention_reaches_both_the_source_and_home`、
//! `a_waiting_run_sends_its_intervention_to_the_chat_and_home`、
//! `an_http_run_sends_its_intervention_to_home_only`、
//! `a_round_stopped_at_its_first_call_still_finishes_the_rest`（"停在结果不明上的半轮续跑"：结果不明
//! 现在直接交给模型，由 runtime 的 durable 测试覆盖）。

use std::sync::Arc;

use komo_gateway::channels::ChannelSender;
use komo_gateway::service::test_support::Gate;
use komo_kernel::protocol::InboundAck;
use komo_kernel::traits::Notifier;
use komo_kernel::types::chat::{
    ChannelPeer, ChannelPlatform, DeliveryState, DeliveryTarget, Outbound, PeerId,
};
use komo_kernel::types::status::RunState;

use crate::harness::{
    FakeLlm, FixedFactory, GatewayBuilder, MemSender, TestGateway, call_round, config_toml,
    eventually, feishu_block, inbound, telegram_block, telegram_config, text_round,
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
        .durable()
        .fake_llm(FakeLlm::finisher("好"))
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
    let (gateway, _sender) = TestGateway::start_durable().await;

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
        gateway.durable_origins().await.is_empty(),
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
    let (gateway, _sender) = TestGateway::start_durable().await;
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
    let (gateway, sender) = TestGateway::start_durable().await;

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
    .durable()
    .fake_llm(FakeLlm::finisher("好"))
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
        .durable()
        .fake_llm(FakeLlm::finisher("好"))
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
        .durable()
        .fake_llm(FakeLlm::finisher("好"))
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

    // 新发送口被扣住：每条补发都停在平台往返上，放开之前一条也发不完。同步补发的话，
    // 重启就永远回不来。期限只是兜底，取客户端的就绪超时：负载下光装配就要
    // 好几秒，拿墙钟去量"够不够快"量的是机器忙不忙。
    let second = MemSender::new(ChannelPlatform::Telegram);
    second.hold();
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        gateway.restart_with(
            &telegram_config("111"),
            vec![FixedFactory::sender_only(
                Arc::clone(&second) as Arc<dyn ChannelSender>
            )],
        ),
    )
    .await
    .expect("补发扣着没放，就绪却在等它——补发又回到就绪之前了");
    assert!(second.sent().is_empty(), "就绪的时候补发还一条都没发完");

    // 补发照样发生，只是晚一步。放开它再等。
    second.release();
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

    let origins: Vec<String> = gateway.durable_origins().await;
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

    let mut origins: Vec<String> = gateway.durable_origins().await;
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
    let (gateway, _sender) = TestGateway::start_durable().await;
    let first = gateway.handle(operator_dm("在", "telegram:1")).await;
    let InboundAck::Queued { session, .. } = first else {
        panic!("{first:?}");
    };

    let ack = gateway.handle(operator_dm("/new", "telegram:2")).await;
    let InboundAck::Replied { text } = ack else {
        panic!("{ack:?}");
    };
    assert!(text.contains("新的一段"), "{text}");

    let (home, _) = gateway
        .state()
        .core()
        .unwrap()
        .main_session("assistant")
        .await
        .unwrap();
    assert_eq!(home.id, session.as_str(), "还是同一个 home session");
    assert_eq!(gateway.durable_origins().await.len(), 1);

    // 事件流上有那条边界。
    let (status, body) = gateway
        .get(&format!("/v1/sessions/{session}/events?from=0"))
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("conversation.boundary"), "{body}");
}

/// §11.3 命令表：`/status` / `/pending` / `/cancel` 都有回执，且都只对操作者生效。
///
/// 改了断言：Legacy 让 run 停在一次"结果不明"（`unsure`）的介入上，`/pending` 列出它、`/status` 数出
/// "结果不明 1"、`/cancel` 取消停着的那条。新存储里结果不明直接交给模型，intervention 清单只剩
/// faulted 任务（W4a 接），所以这里让 run 忙在 `gate` 上：`/status` 报当前任务（执行中）与待处理 0，
/// `/pending` 说没有，`/cancel` 停下这一轮并说出 run 与它现在的状态（已取消）。
#[tokio::test]
async fn the_command_table_is_operator_only() {
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gate = Gate::new();
    let gateway = GatewayBuilder::new(&telegram_config("111"))
        .durable()
        .fake_llm(FakeLlm::new(vec![vec![call_round(
            1,
            "pc-1",
            "gate",
            serde_json::json!({}),
        )]]))
        .tool(gate.tool())
        .factory(FixedFactory::sender_only(
            Arc::clone(&sender) as Arc<dyn ChannelSender>
        ))
        .start()
        .await;
    let ack = gateway.handle(operator_dm("试一下", "telegram:0")).await;
    let InboundAck::Queued { run, .. } = ack else {
        panic!("{ack:?}");
    };
    gate.wait_entered(1).await;

    let pending = gateway.handle(operator_dm("/pending", "telegram:1")).await;
    assert!(
        matches!(&pending, InboundAck::Replied { text } if text.contains("现在没有待处理的 Intervention")),
        "{pending:?}"
    );

    let status = gateway.handle(operator_dm("/status", "telegram:2")).await;
    // §7.5：待处理是两类合起来的一张清单，`/status` 报总数与各类的分布。
    assert!(
        matches!(
            &status,
            InboundAck::Replied { text }
                if text.contains(run.as_str())
                    && text.contains("执行中")
                    && text.contains("待处理（共 0 条）：结果不明 0 · 前提没了 0")
        ),
        "{status:?}"
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
        "命令只接受操作者：{stranger:?}"
    );

    let cancel = gateway.handle(operator_dm("/cancel", "telegram:3")).await;
    assert!(
        matches!(&cancel, InboundAck::Replied { text } if text.contains(run.as_str()) && text.contains("已取消")),
        "/cancel 取消在跑的那条：{cancel:?}"
    );
}

/// §11.2：`allow_from` 为空的渠道"只出不进"——还能作 home chat 收投递，但没人能下指令。
#[tokio::test]
async fn an_empty_allow_list_still_receives_but_nobody_can_speak() {
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gateway = GatewayBuilder::new(&telegram_config(""))
        .durable()
        .fake_llm(FakeLlm::finisher("好"))
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

/// §8.4：前一个 Run 还在跑时，**同一 Session 的下一条消息不越过它**。
///
/// 越过去不是"顺序不好看"：停住的那半轮里那次调用还没有结果，回放窗口只能带着一个没有输出的
/// `function_call` 发给模型，provider 直接 400（`No tool output found for tool call …`）。
///
/// 改了断言：Legacy 让前一条停在介入上（`waiting + intervention`），后一条是 `waiting + dependency`
/// 并说出在等哪一条；新存储里后一条是收件箱里排着的 follow-up（`queued`），前一条收尾的边界才把它放进
/// 上下文——前一条这里忙在 `gate` 上。其余不变：排着的那条不走到模型，前一条跑完后它才轮到，
/// 交给模型的每一份转写都不把用户消息夹在没有输出的调用中间，后一条的窗口里看得见前一条说了什么。
#[tokio::test]
async fn a_message_behind_a_waiting_run_waits_instead_of_overtaking() {
    let gate = Gate::new();
    let llm = FakeLlm::new(vec![
        // 第一段：调 `gate` 停在门上，门开之后收尾。
        vec![
            call_round(1, "pc-1", "gate", serde_json::json!({})),
            text_round(2, "跑完了。"),
        ],
        // 排队那条 Run 的那一段。
        vec![text_round(1, "y")],
    ]);
    let gateway = GatewayBuilder::new(&telegram_config("111"))
        .durable()
        .fake_llm(Arc::clone(&llm))
        .tool(gate.tool())
        .start()
        .await;
    let (_, body) = gateway.get("/v1/home-session").await;
    let session = serde_json::from_str::<komo_kernel::protocol::http::SessionSummary>(&body)
        .expect("home session")
        .session;
    let submit = |key: &'static str, text: &'static str| {
        let session = session.clone();
        let gateway = &gateway;
        async move {
            let (code, body) = gateway
                .post(
                    &format!("/v1/sessions/{session}/runs"),
                    serde_json::json!({ "request_key": key, "text": text }),
                )
                .await;
            assert_eq!(code, 200, "{body}");
            serde_json::from_str::<komo_kernel::protocol::http::SubmitRunResponse>(&body)
                .expect("回执")
                .run
        }
    };

    let first = submit("overtake-1", "跑一下 echo").await;
    gate.wait_entered(1).await;
    let turns_before = llm.turns();

    // 同一个 Session 再来一条：前一个没结束，它不许越过。
    let second = submit("overtake-2", "y").await;
    let queued = gateway
        .state()
        .core()
        .unwrap()
        .submission_state(komo_kernel::durable::legacy_ids::submission_of(&second).unwrap())
        .await
        .unwrap();
    assert_eq!(
        queued,
        RunState::Queued,
        "前一个还在跑，后一个只在收件箱里排着"
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(llm.turns(), turns_before, "排队的这条不该走到模型");

    // 前一个跑完，后一个才轮到。
    gate.release();
    gateway.wait_run(&first).await;
    gateway.wait_run(&second).await;

    // 而且交给模型的每一份转写都过得了 provider 那一关。
    let requests = llm.requests.lock().expect("脚本模型").clone();
    assert_transcripts_close_every_call(&requests);

    // §8.3：后一条 Run 的窗口里要看得见前一条说了什么、最后答了什么——用户那句"接着上面
    // 说"指的就是它，少了这两句，模型只能靠猜。
    let turns: Vec<&str> = requests
        .iter()
        .filter_map(|request| {
            let texts: Vec<&str> = request
                .messages
                .iter()
                .filter_map(|message| message.text.as_deref())
                .collect();
            texts.contains(&"y").then_some(texts)
        })
        .next_back()
        .expect("第二条 Run 的这一轮到过模型");
    assert!(
        turns.contains(&"跑一下 echo"),
        "上一轮的用户输入：{turns:?}"
    );
    assert!(turns.contains(&"跑完了。"), "上一轮的最后回复：{turns:?}");
    assert!(turns.contains(&"y"), "这一轮的输入：{turns:?}");
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

/// 同一会话里，前一条停在 `gate` 上（`running`、占着名额）时，`/cancel` 取消的是它，
/// 门不必开——取消不能卡在一次还没返回的调用上。
#[tokio::test]
async fn cancel_stops_a_run_that_is_still_inside_a_call() {
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gate = Gate::new();
    let gateway = GatewayBuilder::new(&telegram_config("111"))
        .durable()
        .fake_llm(FakeLlm::new(vec![vec![
            call_round(1, "pc-1", "gate", serde_json::json!({})),
            text_round(2, "不该走到这里。"),
        ]]))
        .tool(gate.tool())
        .factory(FixedFactory::sender_only(
            Arc::clone(&sender) as Arc<dyn ChannelSender>
        ))
        .start()
        .await;
    let ack = gateway.handle(operator_dm("等着", "telegram:1")).await;
    let InboundAck::Queued { run, .. } = ack else {
        panic!("{ack:?}");
    };
    gate.wait_entered(1).await;

    let cancel = gateway.handle(operator_dm("/cancel", "telegram:2")).await;
    assert!(
        matches!(&cancel, InboundAck::Replied { text } if text.contains(run.as_str())),
        "{cancel:?}"
    );
    // 取消是异步的：等这条 run 的输入结算，它的状态是已取消（`aborted`），门没有开过。
    let record = gateway.wait_run(&run).await;
    assert_eq!(
        komo_gateway::service::durable::runs::state_of_submission(&record),
        RunState::Cancelled,
        "没取消掉：{record:?}"
    );
}
