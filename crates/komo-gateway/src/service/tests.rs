//! 验收（§14 第 4 阶段那一行，以及 §3 的热重载、§11.4 的投递、§13.1 的 SSE）。
//!
//! 每个测试断言的是**行为**，不是"状态变成了什么"：一条消息重发只产生一个 Run、
//! 一条被拒绝的消息在账本与投递表里都查不到、非法配置重载之后旧名单仍然生效。

use std::sync::Arc;

use komo_kernel::protocol::{InboundAck, InboundMessage};
use komo_kernel::traits::Inbound;
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform, DeliveryTarget, Outbound, PeerId};
use komo_kernel::types::ids::{RequestKey, RunId, SessionId};
use komo_kernel::types::status::{RunState, WaitReason};

use crate::dispatcher::inbound;
use crate::service::test_support::{TestGateway, config_toml, telegram_config};

fn operator_dm(text: &str, key: &str) -> InboundMessage {
    inbound(ChannelPlatform::Telegram, "111", "111", text, key, true)
}

/// ① 同一个 `request_key` 重发只产生一个 Run。
#[tokio::test]
async fn a_replayed_message_produces_one_run() {
    let gateway = TestGateway::start().await;
    let first = gateway
        .dispatcher()
        .handle(operator_dm("帮我看看", "telegram:42"))
        .await
        .expect("第一条");
    let InboundAck::Queued { session, run } = first else {
        panic!("第一条应该排队：{first:?}");
    };

    // 平台重投：同一个 update_id 再来一次。
    let again = gateway
        .dispatcher()
        .handle(operator_dm("帮我看看", "telegram:42"))
        .await
        .expect("重投");
    assert_eq!(
        again,
        InboundAck::Duplicate {
            run: Some(run.clone())
        },
        "重投不该开第二个 Run"
    );

    let runs = komo_store::repos::runs::list_for_session(&gateway.state().db, &session)
        .await
        .expect("读得到");
    assert_eq!(runs.len(), 1, "账本上只有一个 Run");
}

/// 聊天渠道的普通消息以插话受理（§6，2026-10-23）：宿主 Run 忙着跑工具时，排队的消息
/// 要靠 `run.accepted` 上的 `interject` 才并得进去——实测不带它排过 18–22 分钟。
#[tokio::test]
async fn a_plain_chat_message_is_accepted_as_an_interjection() {
    let gateway = TestGateway::start().await;
    let ack = gateway
        .dispatcher()
        .handle(operator_dm("稍等", "telegram:50"))
        .await
        .expect("处理了");
    let InboundAck::Queued { session, run } = ack else {
        panic!("应该排队：{ack:?}");
    };

    let paths = gateway.state().ledgers.paths_for(&session);
    let (events, _) = komo_store::session_log::read_events(
        &paths,
        &session,
        komo_kernel::types::ids::Seq::ZERO,
        0,
    )
    .await
    .expect("读得出会话");
    let accepted = events
        .into_iter()
        .find_map(|event| match event.payload {
            komo_kernel::events::EventPayload::RunAccepted(body)
                if event.run.as_ref() == Some(&run) =>
            {
                Some(body)
            }
            _ => None,
        })
        .expect("这条 Run 受理过");
    assert!(accepted.interject, "渠道的普通消息带插话标记");
}

/// 渠道带来的话题上下文：普通文本把它拼进提交的正文（引用块 + "发给你的消息："），
/// 命令只解析原文、不受影响（§11.3 话题上下文）。
#[tokio::test]
async fn thread_context_is_prepended_to_plain_text_but_never_to_commands() {
    let gateway = TestGateway::start().await;

    let mut plain = operator_dm("看看 owl 的两条 Blocking", "telegram:60");
    plain.context = Some("张三 10:23：B1 在 Drop，B2 在 escape".into());
    let ack = gateway.dispatcher().handle(plain).await.expect("处理了");
    let InboundAck::Queued { session, run } = ack else {
        panic!("应该排队：{ack:?}");
    };
    let paths = gateway.state().ledgers.paths_for(&session);
    let (events, _) = komo_store::session_log::read_events(
        &paths,
        &session,
        komo_kernel::types::ids::Seq::ZERO,
        0,
    )
    .await
    .expect("读得出会话");
    let text = events
        .into_iter()
        .find_map(|event| match event.payload {
            komo_kernel::events::EventPayload::RunAccepted(body)
                if event.run.as_ref() == Some(&run) =>
            {
                body.text
            }
            _ => None,
        })
        .expect("这条 Run 受理过");
    assert!(text.starts_with("> 【话题上下文】"), "{text}");
    assert!(
        text.contains("> 张三 10:23：B1 在 Drop，B2 在 escape\n"),
        "{text}"
    );
    assert!(
        text.ends_with("发给你的消息：\n看看 owl 的两条 Blocking"),
        "{text}"
    );

    // 命令：照旧按原文解析，上下文被丢掉，不起 Run。
    let mut status = operator_dm("/status", "telegram:61");
    status.context = Some("张三 10:23：这段不该影响命令".into());
    let ack = gateway.dispatcher().handle(status).await.expect("处理了");
    assert!(matches!(ack, InboundAck::Replied { .. }), "{ack:?}");
    let runs = komo_store::repos::runs::list_for_session(&gateway.state().db, &session)
        .await
        .expect("读得到");
    assert_eq!(runs.len(), 1, "命令没有起第二个 Run");
}

/// ① 的另一半：重发的 `/answer` 只答一次。
///
/// 停住 Run 的是一次"结果不明"（测试工具 `unsure`，§7.5 的 `verify`）：平台重投同一条
/// 命令，回执相同，那次调用也只被收尾一次——核对之后不重跑。
#[tokio::test]
async fn a_replayed_answer_answers_once() {
    use crate::service::test_support::Unsure;
    use crate::service::test_support::harness::{FakeLlm, Home, call_round, text_round};

    let home = Home::new();
    let unsure = Unsure::new();
    home.install_tools(vec![unsure.tool()]);
    let gateway = home
        .start(FakeLlm::new(vec![vec![
            call_round(1, "pc-1", "unsure", serde_json::json!({})),
            text_round(2, "好了。"),
        ]]) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gateway.open_session().await;
    let run = gateway.submit(&session, "answer-1", "试一下").await.run;
    let pending = gateway.wait_intervention(&run).await;

    let command = format!("/answer {} satisfied", pending.handle);
    let first = gateway
        .dispatcher()
        .handle(operator_dm(&command, "telegram:7"))
        .await
        .expect("第一次");
    let InboundAck::Replied { text } = &first else {
        panic!("命令要有回执：{first:?}");
    };
    assert!(!text.contains("已经答复过"), "第一次不是重复：{text}");

    // 平台重投同一条命令。
    let again = gateway
        .dispatcher()
        .handle(operator_dm(&command, "telegram:7"))
        .await
        .expect("重投");
    assert_eq!(again, first, "重投拿到的是同一个回执");

    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed);
    assert_eq!(unsure.ran(), 1, "核对之后不重跑");
}

/// ② 不在 `allow_from` 的发送者被拒、`/id` 对他可用、且**账本与 deliveries 无任何记录**。
#[tokio::test]
async fn a_stranger_is_refused_and_leaves_no_trace() {
    let gateway = TestGateway::start().await;
    let stranger = inbound(
        ChannelPlatform::Telegram,
        "999",
        "999",
        "在吗",
        "telegram:1",
        true,
    );

    let ack = gateway.dispatcher().handle(stranger).await.expect("处理了");
    let InboundAck::Rejected { hint } = ack else {
        panic!("不在名单里的应该被拒：{ack:?}");
    };
    // 提示里带着他在这个平台的 id，操作者抄进 allow_from 即可。
    assert!(hint.contains("999"), "{hint}");
    assert!(hint.contains("allow_from"), "{hint}");

    // `/id` 对任何人可用——这是唯一不要求操作者身份的命令。
    let id = gateway
        .dispatcher()
        .handle(inbound(
            ChannelPlatform::Telegram,
            "999",
            "999",
            "/id",
            "telegram:2",
            true,
        ))
        .await
        .expect("处理了");
    assert!(
        matches!(&id, InboundAck::Replied { text } if text.contains("telegram:999")),
        "{id:?}"
    );

    // 不留任何记录：没有会话、没有 Run、没有投递。
    let sessions = komo_store::repos::session::list(&gateway.state().db, false)
        .await
        .expect("读得到");
    assert!(
        sessions.is_empty(),
        "被拒绝的消息不该建出会话：{sessions:?}"
    );
    let pending = gateway
        .state()
        .notifier
        .log()
        .pending(None)
        .await
        .expect("读得到");
    assert!(pending.is_empty(), "也不该留下投递记录");
}

/// ③ 改 `allow_from` 保存后重载，下一条消息按新名单判定（不重启）。
#[tokio::test]
async fn a_reloaded_allow_list_decides_the_next_message() {
    let gateway = TestGateway::start().await;
    let newcomer = |key: &str| inbound(ChannelPlatform::Telegram, "222", "222", "在吗", key, true);

    let before = gateway
        .dispatcher()
        .handle(newcomer("telegram:1"))
        .await
        .unwrap();
    assert!(matches!(before, InboundAck::Rejected { .. }), "{before:?}");

    gateway.write_config(&telegram_config("111, 222"));
    crate::reload::reload(gateway.state())
        .await
        .expect("装得上");

    let after = gateway
        .dispatcher()
        .handle(newcomer("telegram:2"))
        .await
        .unwrap();
    assert!(
        matches!(after, InboundAck::Queued { .. }),
        "改完名单下一条消息就该进 Run：{after:?}"
    );
}

/// 改 `[memory].model` 后重载：记忆那一层下一次用的就是新模型（不重启）。
#[tokio::test]
async fn a_reloaded_memory_model_reaches_the_memory_manager() {
    let gateway = TestGateway::start().await;
    assert_eq!(gateway.state().memories.model().model, "gpt-test");

    gateway.write_config(&config_toml(
        r#"model = "mem"

[model.mem]
type = "completion"
api_backend = "responses"
base_url = "https://llm.example.com/v1"
model = "memory-next"
api_key_env = "KOMO_LLM_API_KEY"
"#,
    ));
    crate::reload::reload(gateway.state())
        .await
        .expect("装得上");

    assert_eq!(gateway.state().memories.model().model, "memory-next");
}

/// ④ 非法配置重载失败：旧配置继续生效，home chat 收到错误。
#[tokio::test]
async fn an_invalid_reload_keeps_the_old_config_and_says_so() {
    let gateway = TestGateway::start().await;
    let before = gateway.state().snapshot().model.model.clone();

    // `listen` 不是"地址:端口"——校验里的一个 Error。
    gateway.write_config(&config_toml(
        r#"
[gateway]
listen = "这不是一个地址"

[channels.telegram]
enabled = true
allow_from = [111]
home_chat = 111
"#,
    ));

    let error = crate::reload::reload(gateway.state())
        .await
        .expect_err("装不上");
    assert_eq!(
        error.code(),
        komo_kernel::protocol::http::ErrorCode::ConfigInvalid
    );
    assert!(!error.error.keys.is_empty(), "错误要带键名定位");

    // 旧快照原样保留。
    assert_eq!(gateway.state().snapshot().model.model, before);
    assert!(
        gateway
            .state()
            .snapshot()
            .channels
            .telegram
            .is_operator(&PeerId::new("111")),
        "旧名单仍然生效"
    );

    // home chat 收到了那条错误。
    let sent = gateway.channel.sent();
    assert!(
        sent.iter().any(|message| matches!(
            &message.outbound,
            Outbound::Text { text } if text.contains("配置没装上")
        )),
        "home chat 要收到具体错误：{sent:?}"
    );
}

/// 编辑器每次自动保存都触发一次重载：同一条错误只投一次，装上之后说一声。
#[tokio::test]
async fn a_repeated_reload_error_is_delivered_once_and_recovery_is_announced() {
    let gateway = TestGateway::start().await;
    let broken = config_toml(
        r#"
[gateway]
listen = "这不是一个地址"
"#,
    );
    let count = |gateway: &TestGateway, needle: &str| {
        gateway
            .channel
            .sent()
            .iter()
            .filter(|message| {
                matches!(&message.outbound, Outbound::Text { text } if text.contains(needle))
            })
            .count()
    };

    gateway.write_config(&broken);
    crate::reload::reload(gateway.state()).await.unwrap_err();
    gateway.write_config(&broken);
    crate::reload::reload(gateway.state()).await.unwrap_err();
    assert_eq!(count(&gateway, "配置没装上"), 1, "同一条错误不重复投");

    gateway.write_config(&telegram_config("111"));
    crate::reload::reload(gateway.state())
        .await
        .expect("装得上");
    assert_eq!(count(&gateway, "配置已装上"), 1, "恢复要说一声");

    gateway.write_config(&telegram_config("111"));
    crate::reload::reload(gateway.state())
        .await
        .expect("装得上");
    assert_eq!(count(&gateway, "配置已装上"), 1, "没出过错就不用说");
}

/// ⑤ 介入投到来源会话与 home chat；第二个答复得到"已经答复过了"。
#[tokio::test]
async fn an_intervention_goes_to_both_the_source_and_home_and_is_answered_once() {
    use crate::channels::ChannelSender;
    use crate::service::test_support::Unsure;
    use crate::service::test_support::harness::{
        FakeLlm, FixedFactory, GatewayBuilder, MemSender, call_round, text_round,
    };

    // home chat 是 111，来源会话是群 222。
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let unsure = Unsure::new();
    let gateway = GatewayBuilder::new(&telegram_config("111"))
        .factory(FixedFactory::sender_only(
            Arc::clone(&sender) as Arc<dyn ChannelSender>
        ))
        .llm(FakeLlm::new(vec![vec![
            call_round(1, "pc-1", "unsure", serde_json::json!({})),
            text_round(2, "好了。"),
        ]]))
        .tool(unsure.tool())
        .start()
        .await;

    let ack = gateway
        .handle(inbound(
            ChannelPlatform::Telegram,
            "222",
            "111",
            "跑一下",
            "telegram:500",
            false,
        ))
        .await;
    let InboundAck::Queued { run, .. } = ack else {
        panic!("群里的话要起一个 Run：{ack:?}");
    };
    let pending = gateway.wait_intervention(&run).await;

    crate::service::test_support::harness::eventually(
        "来源会话与 home chat 都收到",
        || {
            let targets: Vec<String> = sender
                .sent()
                .iter()
                .filter(|message| matches!(message.outbound, Outbound::NeedsAttention { .. }))
                .map(|message| message.peer.chat_id.to_string())
                .collect();
            targets.contains(&"222".to_string()) && targets.contains(&"111".to_string())
        },
    )
    .await;

    // 第一个答复。
    let first: komo_kernel::protocol::http::InterventionAnswerResponse =
        serde_json::from_str(&gateway.answer(&pending.handle, "satisfied").await).expect("回执");
    assert!(!first.already_answered);

    // 第二个答复得到"已经答复过了"，且不改变结论。
    let second: komo_kernel::protocol::http::InterventionAnswerResponse =
        serde_json::from_str(&gateway.answer(&pending.handle, "abandon").await).expect("回执");
    assert!(
        second.already_answered,
        "第二个答复应当得到原结论：{second:?}"
    );
    assert_ne!(
        second.run_state,
        Some(RunState::Abandoned),
        "结论没有被第二次答复改写"
    );
    assert_eq!(unsure.ran(), 1);
}

/// ⑥ `Deferred` 的投递在下一条入站消息时先被冲刷。
#[tokio::test]
async fn a_deferred_delivery_is_flushed_before_the_next_message() {
    let gateway = TestGateway::start().await;
    // 微信那条路径：此刻没有回复令牌。
    gateway.channel.defer(true);

    let peer = ChannelPeer::new(ChannelPlatform::Telegram, "111");
    let delivery = gateway
        .state()
        .notifier
        .deliver(
            &DeliveryTarget::to_peer(peer.clone()),
            Outbound::Text {
                text: "要批一下".into(),
            },
        )
        .await;
    use komo_kernel::traits::Notifier;
    let delivery = delivery.expect("登记得上");
    assert_eq!(
        delivery.state,
        komo_kernel::types::chat::DeliveryState::Deferred
    );
    assert!(
        gateway.channel.sent().is_empty(),
        "推不出去就没有送出这一条"
    );

    // 用户发了消息，令牌有了。
    gateway.channel.defer(false);
    gateway
        .dispatcher()
        .handle(operator_dm("在", "telegram:9"))
        .await
        .expect("处理了");

    let sent = gateway.channel.sent();
    assert!(
        sent.iter().any(
            |message| matches!(&message.outbound, Outbound::Text { text } if text == "要批一下")
        ),
        "积压的那一条要先送到：{sent:?}"
    );
    assert!(
        gateway
            .state()
            .notifier
            .log()
            .pending(None)
            .await
            .unwrap()
            .is_empty(),
        "冲刷之后不再 pending"
    );
}

/// ⑦ 多个进程同时启动只有一个拿到锁：第二台 Gateway 起不来。
#[tokio::test]
async fn only_one_gateway_holds_a_data_directory() {
    let gateway = TestGateway::start().await;
    let second = crate::service::start(crate::service::ServiceOptions {
        home: Some(gateway.home.path().to_path_buf()),
        listen: Some("127.0.0.1:0".into()),
        channels: Vec::new(),
        storage: crate::service::StorageMode::Legacy,
        llm: None,
        stream: None,
        embeddings: None,
        shared_home: None,
        telemetry: None,
    })
    .await;
    let error = second.expect_err("第二台起不来");
    assert!(
        matches!(error, crate::service::ServiceError::Lock(_)),
        "{error:?}"
    );
    assert!(
        gateway.home.path().join(crate::lock::LOCK_PATH).exists(),
        "仍然有效的锁没有被删掉"
    );
}

/// ⑧ SSE 从 `Last-Event-ID` 续读，且先历史后直播。
#[tokio::test]
async fn sse_replays_history_from_a_cursor_then_goes_live() {
    let gateway = TestGateway::start().await;
    let ack = gateway
        .dispatcher()
        .handle(operator_dm("第一条", "telegram:1"))
        .await
        .unwrap();
    let InboundAck::Queued { session, .. } = ack else {
        panic!("{ack:?}");
    };

    // 从头补读：拿得到第一条输入。
    let stream = reqwest::Client::new()
        .get(format!(
            "{}/v1/sessions/{session}/events",
            gateway.base_url()
        ))
        .bearer_auth(gateway.token())
        .header("accept", "text/event-stream")
        .header("last-event-id", "1")
        .send()
        .await
        .expect("连得上");
    assert_eq!(stream.status(), 200);

    let mut body = stream.bytes_stream();
    use futures_util::StreamExt;
    let mut seen = String::new();
    // 历史那几帧是立刻就有的。
    while let Ok(Some(chunk)) =
        tokio::time::timeout(std::time::Duration::from_secs(5), body.next()).await
    {
        seen.push_str(&String::from_utf8_lossy(&chunk.expect("一块")));
        if seen.contains("run.queued") {
            break;
        }
    }
    assert!(seen.contains("id: 2"), "游标之后才补读：{seen}");
    assert!(!seen.contains("id: 1"), "游标那一条不再给一遍：{seen}");

    // 直播：现在写一条新的，同一条连接上就能收到。
    let seq = gateway
        .state()
        .routed_boundary(&session)
        .await
        .expect("写得下");
    let mut live = String::new();
    while let Ok(Some(chunk)) =
        tokio::time::timeout(std::time::Duration::from_secs(5), body.next()).await
    {
        live.push_str(&String::from_utf8_lossy(&chunk.expect("一块")));
        if live.contains("conversation.boundary") {
            break;
        }
    }
    assert!(
        live.contains(&format!("id: {seq}")),
        "补读之后接直播：{live}"
    );
}

/// ⑨ 端到端：提交输入 → Run 完成 → SSE 上有最终消息。
#[tokio::test]
async fn an_end_to_end_turn_completes_and_reaches_the_event_stream() {
    use komo_kernel::test_support::ScriptedLlm;
    use komo_kernel::types::turn::Round;

    let llm = Arc::new(ScriptedLlm::new(vec![vec![Round {
        round: 1,
        text: Some("好了，两加二等于四。".into()),
        tool_calls: Vec::new(),
        provider_blocks: None,
        usage: Default::default(),
        truncated: false,
    }]]));
    let gateway = TestGateway::with(&telegram_config("111"), Some(llm)).await;

    // 创建会话（走 HTTP，和 `komo` 那条命令一样）。
    let (status, body) = gateway.post("/v1/sessions", serde_json::json!({})).await;
    assert_eq!(status, 200, "{body}");
    let session: komo_kernel::protocol::http::SessionSummary =
        serde_json::from_str(&body).expect("会话");

    // 提交输入。
    let (status, body) = gateway
        .post(
            &format!("/v1/sessions/{}/runs", session.session),
            serde_json::json!({"request_key": "cli-1", "text": "二加二等于几"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let submitted: komo_kernel::protocol::http::SubmitRunResponse =
        serde_json::from_str(&body).expect("提交");

    // 等它跑完。
    let detail = wait_for_run(&gateway, &submitted.run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    assert_eq!(
        detail.final_message.as_deref(),
        Some("好了，两加二等于四。")
    );

    // 事件流上有那条 assistant 消息（补读，不依赖有没有人在线）。
    let (status, body) = gateway
        .get(&format!("/v1/sessions/{}/events?from=0", session.session))
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("message.assistant"), "{body}");
    assert!(body.contains("两加二等于四"), "{body}");
}

/// 认证：除 `/healthz` 外统一 Bearer。
#[tokio::test]
async fn everything_but_health_needs_the_token() {
    let gateway = TestGateway::durable(
        &telegram_config("111"),
        crate::service::test_support::harness::FakeLlm::finisher("好"),
    )
    .await;

    let health = reqwest::get(format!("{}/healthz", gateway.base_url()))
        .await
        .expect("健康检查不要认证");
    assert_eq!(health.status(), 200);

    let refused = reqwest::get(format!("{}/v1/sessions", gateway.base_url()))
        .await
        .expect("发得出去");
    assert_eq!(refused.status(), 401);
    let body = refused.text().await.unwrap_or_default();
    assert!(body.contains("unauthorized"), "{body}");

    let (status, _) = gateway.get("/v1/sessions").await;
    assert_eq!(status, 200, "带上令牌就通过");
}

/// 幂等请求键：同键同内容返回原结果，同键不同内容 409。
#[tokio::test]
async fn the_same_request_key_with_other_content_is_a_conflict() {
    let gateway = TestGateway::durable(
        &telegram_config("111"),
        crate::service::test_support::harness::FakeLlm::finisher("好"),
    )
    .await;
    let (_, body) = gateway.post("/v1/sessions", serde_json::json!({})).await;
    let session: komo_kernel::protocol::http::SessionSummary = serde_json::from_str(&body).unwrap();

    let first = gateway
        .post(
            &format!("/v1/sessions/{}/runs", session.session),
            serde_json::json!({"request_key": "k-1", "text": "一"}),
        )
        .await;
    assert_eq!(first.0, 200, "{}", first.1);

    let same = gateway
        .post(
            &format!("/v1/sessions/{}/runs", session.session),
            serde_json::json!({"request_key": "k-1", "text": "一"}),
        )
        .await;
    assert_eq!(same.0, 200, "{}", same.1);
    assert!(same.1.contains("\"deduplicated\":true"), "{}", same.1);

    let different = gateway
        .post(
            &format!("/v1/sessions/{}/runs", session.session),
            serde_json::json!({"request_key": "k-1", "text": "二"}),
        )
        .await;
    assert_eq!(different.0, 409, "{}", different.1);
    assert!(
        different.1.contains("request_key_conflict"),
        "{}",
        different.1
    );
}

/// `/new` 在当前会话上追加一条边界，**不切 Session**。
///
/// Durable 下的 `/new` 有两个入口：HTTP 的 `POST /boundary`（TUI 用）与聊天渠道的 `/new`
/// （`dispatcher_v2`，W3a 并进来：原先分开的 `_through_chat_legacy` 只测了回执与同一个 home
/// session，这里的断言更强——边界真的落进同一个会话的历史）。
#[tokio::test]
async fn slash_new_appends_a_boundary_to_the_same_session() {
    use crate::service::test_support::harness::FakeLlm;
    use komo_kernel::protocol::http::{BoundaryResponse, SessionSummary, SubmitRunResponse};
    use komo_kernel::session::EntryKind;

    let gateway = TestGateway::durable(&telegram_config("111"), FakeLlm::finisher("好")).await;
    let home = |body: String| {
        serde_json::from_str::<SessionSummary>(&body)
            .unwrap()
            .session
    };
    let (status, body) = gateway.get("/v1/home-session").await;
    assert_eq!(status, 200, "{body}");
    let session = home(body);
    let (status, body) = gateway
        .post(
            &format!("/v1/sessions/{session}/runs"),
            serde_json::json!({"request_key": "home-1", "text": "在"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let run = serde_json::from_str::<SubmitRunResponse>(&body)
        .unwrap()
        .run;
    gateway.wait_run(&run).await;
    let entries_of = |body: String| {
        serde_json::from_str::<komo_kernel::protocol::SessionTreeResponse>(&body)
            .unwrap()
            .entries
    };
    let (_, body) = gateway.get(&format!("/v1/sessions/{session}/tree")).await;
    let before = entries_of(body);

    let (status, body) = gateway
        .post(
            &format!("/v1/sessions/{session}/boundary"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let boundary: BoundaryResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(boundary.session, session);

    // 还是同一个 home session，边界落在它的历史里（"自此重新开始"在 pi 里是一条压缩）。
    let (_, body) = gateway.get("/v1/home-session").await;
    assert_eq!(home(body), session);
    let (_, body) = gateway.get(&format!("/v1/sessions/{session}/tree")).await;
    let after = entries_of(body);
    assert!(after.len() > before.len(), "{after:?}");
    assert!(
        after[before.len()..]
            .iter()
            .any(|entry| matches!(entry.kind, EntryKind::Compaction { .. })),
        "{after:?}"
    );

    // 聊天渠道的 `/new`：回执，落在**同一个** home session（操作者私聊就是它），又多一条边界。
    let ack = gateway
        .dispatcher()
        .handle(operator_dm("/new", "telegram:2"))
        .await
        .unwrap();
    assert!(
        matches!(&ack, InboundAck::Replied { text } if text.contains("新的一段")),
        "{ack:?}"
    );
    let (_, body) = gateway.get("/v1/home-session").await;
    assert_eq!(home(body), session, "聊天里的 /new 也不切 Session");
    let (_, body) = gateway.get(&format!("/v1/sessions/{session}/tree")).await;
    let after_chat = entries_of(body);
    assert!(
        after_chat.len() > after.len()
            && after_chat[after.len()..]
                .iter()
                .any(|entry| matches!(entry.kind, EntryKind::Compaction { .. })),
        "{after_chat:?}"
    );
}

// ---------------------------------------------------------------- 工具

/// 等一条 Run 进终态（10 秒超时）。
async fn wait_for_run(
    gateway: &TestGateway,
    run: &RunId,
) -> komo_kernel::protocol::http::RunDetail {
    for _ in 0..200 {
        let (status, body) = gateway.get(&format!("/v1/runs/{run}")).await;
        if status == 200
            && let Ok(detail) =
                serde_json::from_str::<komo_kernel::protocol::http::RunDetail>(&body)
            && detail.summary.state.is_terminal()
        {
            return detail;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("等了 10 秒 run {run} 还没有终态");
}

/// 审批删掉之后（§7），`y` 就是一句话：**有介入挂着也一样**，它不替人答任何东西。
/// `/approve` 不是命令了，答一句帮助，而不是把它交给模型跑一轮。
#[tokio::test]
async fn a_bare_yes_is_just_a_message_even_when_something_is_pending() {
    use crate::service::test_support::Unsure;
    use crate::service::test_support::harness::{FakeLlm, Home, call_round, text_round};

    let home = Home::new();
    let unsure = Unsure::new();
    home.install_tools(vec![unsure.tool()]);
    let gateway = home
        .start(FakeLlm::new(vec![vec![
            call_round(1, "pc-1", "unsure", serde_json::json!({})),
            text_round(2, "好了。"),
        ]]) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gateway
        .state()
        .default_main_session()
        .await
        .expect("home session");
    let run = gateway.submit(&session, "yes-1", "试一下").await.run;
    gateway.wait_intervention(&run).await;

    let ack = gateway
        .dispatcher()
        .handle(operator_dm("y", "telegram:900"))
        .await
        .expect("收下");
    assert!(
        matches!(ack, InboundAck::Queued { .. }),
        "`y` 是回话，不是答复：{ack:?}"
    );
    assert_eq!(
        gateway.db_state(&run).await,
        RunState::Waiting,
        "挂着的那条没有被 `y` 答掉"
    );

    let ack = gateway
        .dispatcher()
        .handle(operator_dm("/approve", "telegram:901"))
        .await
        .expect("收下");
    let InboundAck::Replied { text } = ack else {
        panic!("`/approve` 要答一句帮助：{ack:?}");
    };
    assert!(text.contains("/answer"), "{text}");
}

/// 没有介入时，`y` / `n` 同样走普通消息那条路（不是命令）。
#[tokio::test]
async fn a_bare_yes_with_nothing_pending_is_just_a_message() {
    let gateway = TestGateway::start().await;
    let ack = gateway
        .dispatcher()
        .handle(operator_dm("n", "telegram:901"))
        .await
        .expect("收下");
    assert!(
        matches!(ack, InboundAck::Queued { .. }),
        "`n` 是回话，不该被读成拒绝：{ack:?}"
    );
}

// ---------------------------------------------------------------- §7 没有 Policy

/// **还留着的 `policy.toml` 报一句，但不读**（§7、D13）：一份"shell 一律要问"的旧规则表
/// 放在数据目录里，命令照样直接跑；`komo config check` 说它可以删了，删掉之后不再说。
#[tokio::test]
async fn a_leftover_policy_file_is_reported_but_never_read() {
    use crate::service::test_support::harness::{FakeLlm, Home, call_round, text_round};

    let home = Home::new();
    std::fs::write(
        home.path().join("policy.toml"),
        "default = \"ask\"\n\n[[rules]]\nid = \"ask-shell\"\neffect = \"ask\"\nreason = \"要问我\"\n\n[rules.matcher]\noperations = [\"shell_command\"]\n",
    )
    .expect("写 policy.toml");
    let once = home.workspace().join("leftover.count");
    let command = format!("echo once >> {}", once.display());
    let gateway = home
        .start(FakeLlm::new(vec![vec![
            call_round(1, "pc-1", "bash", serde_json::json!({ "command": command })),
            text_round(2, "做完了。"),
        ]]) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;

    let (code, body) = gateway.get_json("/v1/config/check").await;
    assert_eq!(code, 200, "{body}");
    let keys = |body: &serde_json::Value| -> Vec<String> {
        body["issues"]
            .as_array()
            .map(|issues| {
                issues
                    .iter()
                    .filter_map(|issue| issue["key"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert!(
        keys(&body).contains(&"policy.toml".to_string()),
        "要报出来：{body}"
    );

    let session = gateway.open_session().await;
    let run = gateway.submit(&session, "leftover-1", "跑一下").await.run;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed);
    assert_eq!(
        std::fs::read_to_string(&once)
            .unwrap_or_default()
            .lines()
            .count(),
        1,
        "旧规则表不拦：命令真的跑了"
    );

    std::fs::remove_file(home.path().join("policy.toml")).expect("删掉");
    let (_, body) = gateway.get_json("/v1/config/check").await;
    assert!(
        !keys(&body).contains(&"policy.toml".to_string()),
        "删掉之后不再报：{body}"
    );
}

/// 让编译器盯住这几个在别处用到的类型。
#[allow(dead_code)]
fn unused(_: RequestKey) {}

/// `komo session list` / TUI 列表那一列的标题：第一条消息的前 60 个字符，后来改不掉。
///
/// 没有它，列表里每一行都是「（无标题）」——今晚一排 8 个会话全是这样，认不出哪个是哪个。
#[tokio::test]
async fn the_session_list_shows_the_first_message_as_its_title() {
    let home = crate::service::test_support::harness::Home::new();
    let llm = crate::service::test_support::harness::FakeLlm::new(vec![vec![
        crate::service::test_support::harness::text_round(1, "好。"),
    ]]);
    let gateway = home.start_durable(Arc::clone(&llm)).await;
    let session = gateway.open_session().await;
    gateway
        .submit(&session, "title-1", "空调状态\n顺便看看湿度")
        .await;

    let (status, body) = gateway.get("/v1/sessions").await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("空调状态"),
        "列表里要有标题，读到的是：{}",
        &body[..body.len().min(400)]
    );

    // 第二条消息不改写标题。
    gateway.submit(&session, "title-2", "热水器呢").await;
    let (_, body) = gateway.get("/v1/sessions").await;
    assert!(body.contains("空调状态"), "{body}");
    assert!(!body.contains("热水器呢"), "后来的消息不该改标题：{body}");
}

// ---------------------------------------------------------------- 模型要的就执行
/// **日常命令不问就跑**（§7）：没有 Policy，模型要的、komo 就执行。
#[tokio::test]
async fn an_ordinary_shell_command_runs_without_asking() {
    use crate::service::test_support::harness::{FakeLlm, Home, call_round, text_round};

    let home = Home::new();
    let once = home.workspace().join("auto.count");
    let command = format!("echo once >> {}", once.display());
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-1", "bash", serde_json::json!({ "command": command })),
        text_round(2, "做完了。"),
    ]]);
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gateway.open_session().await;
    let run = gateway.submit(&session, "auto-1", "跑一下 echo").await.run;
    let detail = gateway.wait_terminal(&run).await;

    assert_eq!(detail.summary.state, RunState::Completed);
    assert!(gateway.interventions().await.is_empty(), "没有要人答的");
    assert_eq!(
        std::fs::read_to_string(&once)
            .unwrap_or_default()
            .lines()
            .count(),
        1,
        "命令要真的跑过"
    );
}

/// **危险形状也不问**：边界只能来自执行环境（§7），不来自对命令长相的猜测。
///
/// 靶子是工作区里的一个文件（`rm -rf <workspace>/never-touched`）：先建文件，再看它没了。
#[tokio::test]
async fn even_a_dangerous_command_runs_without_asking() {
    use crate::service::test_support::harness::{FakeLlm, Home, call_round, text_round};

    let home = Home::new();
    let doom = home.workspace().join("never-touched");
    let command = format!("rm -rf {}", doom.display());
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-1", "bash", serde_json::json!({ "command": command })),
        text_round(2, "删掉了。"),
    ]]);
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gateway.open_session().await;
    // `workspaces/` 是 Gateway 启动时建的（§12），所以靶子要在这之后才落得下来。
    std::fs::write(&doom, "x").expect("建个靶子");

    let run = gateway
        .submit(&session, "auto-2", "把这个文件删了")
        .await
        .run;
    let detail = gateway.wait_terminal(&run).await;

    assert_eq!(detail.summary.state, RunState::Completed);
    assert!(gateway.interventions().await.is_empty(), "一条都不该问");
    assert!(!doom.exists(), "命令要真的执行过");
}

/// §7 唯一的硬限制（D7）：`write` 不能改 komo 自己的状态——数据目录下的 `.env` 被拒，
/// 理由点名路径；Run 照常收尾，拒绝是交回模型的一个结果。
#[tokio::test]
async fn write_refuses_komo_s_own_state() {
    use crate::service::test_support::harness::{FakeLlm, Home, call_round, text_round};

    let home = Home::new();
    let env = home.path().join(".env");
    let before = std::fs::read_to_string(&env).expect(".env");
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-1",
            "write",
            serde_json::json!({ "path": env.display().to_string(), "content": "X=1\n" }),
        ),
        text_round(2, "写不了。"),
    ]]);
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gateway.open_session().await;
    let run = gateway.submit(&session, "d7-1", "改一下 .env").await.run;
    let detail = gateway.wait_terminal(&run).await;

    assert_eq!(detail.summary.state, RunState::Completed);
    assert_eq!(
        std::fs::read_to_string(&env).expect(".env"),
        before,
        ".env 一个字都没变"
    );
    // 拒绝是交回模型的一个失败结果，理由点名 komo 自己的状态（§8.10）。
    let refused = llm
        .inputs
        .lock()
        .expect("轮输入")
        .iter()
        .any(|input| format!("{input:?}").contains("komo 自己的状态"));
    assert!(refused, "模型要看到拒绝的理由");
}

// ---------------------------------------------------------------- §8.4 / §7.5 / §8.10

/// §7.5 第 2 条 / §8.4：**等前一条 Run 的那一类不进清单，但说得出在等谁。**
///
/// 「挡着会话的每一条都必须在清单里」说的是**停在人身上**的那一类（`intervention`：
/// `verify` / `blocked`）。`dependency` 在等前一条 Run 跑完，那件事不需要人回答——把它塞进清单
/// 等于让操作者去答一个他答不了的问题；反过来把它藏起来（旧模型里的"排队中"），就正好是
/// §8.4 要消掉的那个"排队二十分钟不知道在等什么"。所以它只在这两格露面：
/// `RunSummary.wait` 与 `SessionSummary.current_wait`。
#[tokio::test]
async fn a_dependency_wait_is_not_an_intervention_but_says_which_run_it_waits_for() {
    use crate::service::test_support::harness::{FakeLlm, Home, call_round};

    let home = Home::with_config(&config_toml(""));
    // 前一条 Run 停在一次"结果不明"上（测试工具 `unsure`）——**确定性**地占住队列；
    // 答复之后两段脚本都用默认的收尾。
    let unsure = crate::service::test_support::Unsure::new();
    home.install_tools(vec![unsure.tool()]);
    let llm = FakeLlm::new(vec![vec![call_round(
        1,
        "a-1",
        "unsure",
        serde_json::json!({}),
    )]]);
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gateway.open_session().await;
    let first = gateway.submit(&session, "dep-1", "先跑这个").await.run;
    let first_wait = gateway.wait_waiting(&first).await;
    assert!(
        matches!(first_wait, WaitReason::Intervention { .. }),
        "前一条该停在一条介入上：{first_wait:?}"
    );

    let second = gateway.submit(&session, "dep-2", "再跑这个").await.run;
    let wait = gateway.wait_waiting(&second).await;
    assert_eq!(
        wait,
        WaitReason::Dependency { run: first.clone() },
        "后一条要说出在等哪一条 Run"
    );

    // 清单里只有停在人身上的那一条。
    let listed = gateway.interventions().await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert!(
        listed.iter().all(|one| one.run.as_ref() != Some(&second)),
        "等前一条 Run 不是等人，不进清单：{listed:?}"
    );

    // 会话详情：`unfinished` 两维都在，`pending` 里没有它。
    let (code, detail) = gateway.get_json(&format!("/v1/sessions/{session}")).await;
    assert_eq!(code, 200, "{detail}");
    let unfinished: Vec<komo_kernel::protocol::http::RunSummary> =
        serde_json::from_value(detail["unfinished"].clone()).expect("unfinished");
    let entry = unfinished
        .iter()
        .find(|one| one.run == second)
        .expect("后一条在 unfinished 里");
    assert_eq!(entry.state, RunState::Waiting);
    assert_eq!(
        entry.wait,
        Some(WaitReason::Dependency { run: first.clone() })
    );
    let pending: Vec<komo_kernel::protocol::http::InterventionSummary> =
        serde_json::from_value(detail["pending"].clone()).expect("pending");
    assert_eq!(
        pending.len(),
        1,
        "这个会话上停着的是前一条那条介入：{pending:?}"
    );
    assert!(
        pending.iter().all(|one| one.run.as_ref() != Some(&second)),
        "后一条（等前一条 Run）不该出现在待处理清单里：{pending:?}"
    );

    // 会话列表那一格（`komo session list` 的"为什么它不动"）。
    let (code, list) = gateway.get_json("/v1/sessions").await;
    assert_eq!(code, 200, "{list}");
    let summaries: Vec<komo_kernel::protocol::http::SessionSummary> =
        serde_json::from_value(list["sessions"].clone()).expect("sessions");
    let summary = summaries
        .iter()
        .find(|one| one.session == session)
        .expect("会话在列表里");
    assert_eq!(
        summary.state,
        komo_kernel::types::status::SessionState::Active
    );
    assert_eq!(summary.current_state, Some(RunState::Waiting));
    assert_eq!(
        summary.current_wait,
        Some(WaitReason::Dependency { run: first.clone() })
    );

    // 前一条进终态 → 这一条自己回队列、跑完（§8.9 第三步，以及终态那一刻的放行）。
    gateway.answer(first.as_str(), "satisfied").await;
    assert_eq!(
        gateway.wait_terminal(&first).await.summary.state,
        RunState::Completed
    );
    assert_eq!(
        gateway.wait_terminal(&second).await.summary.state,
        RunState::Completed,
        "前一条完了，后一条要自己走"
    );
}

/// 直接往库里落一条 `queued` 的 Run：一个崩溃现场留下的"半个事实"。
///
/// 走 store 而不是走 `POST /v1/sessions/{id}/runs`，是因为要测的**正是**"受理这条路已经
/// 被挡住之后，库里那条 Run 还会不会被领走"——受理拒绝（§8.10）与领取守卫是两道不同的
/// 闸，这一条验的是后面那道。
async fn seed_queued_run(
    gateway: &crate::service::test_support::harness::Gw,
    session: &SessionId,
    key: &str,
) -> RunId {
    let state = gateway.state();
    let now = state.clock.now();
    let run = RunId::new_at(now);
    let new = komo_store::repos::runs::NewRun {
        run: run.clone(),
        session: session.clone(),
        request_key: komo_kernel::types::ids::RequestKey::new(key),
        input_hash: "seed".into(),
        source: komo_kernel::types::plan::PlanSource::Interactive {
            session: session.clone(),
        },
        peer: None,
        model: state.snapshot().model.clone(),
        effort: None,
        delegate: None,
        skip_memory: false,
        at: now,
    };
    let event = komo_kernel::types::ids::EventId::new_at(now);
    // 输入序号：受理那一笔用它排同会话的次序（§8.4 的 `dependency` 就看这个）。这里没有
    // 真的追加事件，所以取"这个会话已索引到的下一个"——语义上正是这条输入该占的位置。
    let applied = komo_store::repos::session::get(&state.db, session)
        .await
        .expect("读会话")
        .expect("有这个会话")
        .applied_seq;
    let seq = komo_kernel::types::ids::Seq(applied.0 + 1);
    let db = state.db.clone();
    let for_write = run.clone();
    db.with_write_retry(move |ex| {
        let (new, event, for_write) = (new.clone(), event.clone(), for_write.clone());
        Box::pin(async move {
            komo_store::repos::runs::reserve_in(ex, &new).await?;
            komo_store::repos::runs::mark_queued_in(ex, &for_write, &event, seq, now).await
        }) as komo_store::db::BoxFuture<'_, Result<(), komo_kernel::traits::StoreError>>
    })
    .await
    .expect("落一条 Run");
    run
}

/// §8.9 / §8.10：**已回收会话的 Run 抄不跑，而且对账要说得出为什么。**
///
/// 这是"数据库与内容对不上"最坏的一种：一个 `purged` 的会话上还挂着一条 `queued` 的 Run。
/// 没有领取守卫，它会**照着空上下文跑一轮**——§8.9 说那比停下来更糟。
#[tokio::test]
async fn a_run_in_a_purged_session_is_never_claimed_and_the_reconcile_says_why() {
    use crate::service::test_support::harness::{FakeLlm, Home, text_round};
    use komo_kernel::traits::RunQueue;

    let home = Home::with_config(&config_toml(""));
    // 真跑起来的话这一轮会留下痕迹（`turns`）：一次都不该被调用。
    let llm = FakeLlm::always(vec![text_round(1, "不该跑到这里")]);
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gateway.open_session().await;
    let run = seed_queued_run(&gateway, &session, "purged-1").await;

    // 墓碑先落（§8.10 第 3 条：数据库先提交，内容后删）。
    let db = gateway.state().db.clone();
    let now = gateway.state().clock.now();
    let for_state = session.clone();
    db.with_write_retry(move |ex| {
        let for_state = for_state.clone();
        Box::pin(async move {
            komo_store::repos::session::set_state_in(
                ex,
                &for_state,
                komo_kernel::types::status::SessionState::Active,
                komo_kernel::types::status::SessionState::Deleted,
                now,
            )
            .await
            .map(|_| ())
        }) as komo_store::db::BoxFuture<'_, Result<(), komo_kernel::traits::StoreError>>
    })
    .await
    .expect("逻辑删除");
    assert!(
        komo_store::repos::reconcile::mark_purged(&db, &session, now)
            .await
            .expect("落墓碑")
    );

    // 领取这一侧：候选里没有它，指名领也领不到（§8.9 那条硬约束）。
    let due = gateway.state().queue.due(now, 32).await.expect("读候选");
    assert!(
        !due.contains(&run),
        "purged 会话的 Run 不该出现在候选里：{due:?}"
    );
    assert!(
        gateway
            .state()
            .queue
            .claim_run(&run, &gateway.state().executor)
            .await
            .expect("领取")
            .is_none(),
        "purged 会话的 Run 领不走"
    );
    assert_eq!(llm.turns(), 0, "一次模型都没该被调过");

    // 对账：把它停成一条说得清理由的 `blocked`。
    let (code, body) = gateway
        .post_json("/v1/reconcile", serde_json::json!({}))
        .await;
    assert_eq!(code, 200, "{body}");
    assert!(
        body["blocked"].as_u64().unwrap_or(0) >= 1,
        "对账要判成等人答复：{body}"
    );

    let wait = gateway.wait_waiting(&run).await;
    assert!(
        matches!(wait, WaitReason::Intervention { .. }),
        "要停在干预上：{wait:?}"
    );
    let listed = gateway.interventions().await;
    let entry = listed
        .iter()
        .find(|one| one.run.as_ref() == Some(&run))
        .unwrap_or_else(|| panic!("这一条要在清单里：{listed:?}"));
    assert!(
        entry.question.contains("回收"),
        "理由要说清是哪种不可服务：{entry:?}"
    );
    assert_eq!(llm.turns(), 0, "对账自己不跑模型，也不该让别人跑");
}

/// §8.10：**逻辑删除之后不再接受新输入，而且拒绝要说清楚是哪种状态。**
///
/// `closing` 只是不再收新活（没跑完的照跑），`deleted` 是逻辑删除完成，`purged` 内容已经
/// 回收——三句话对操作者意味着三件不同的事，所以 409 的正文要说得出是哪一种。
///
/// Durable 版（W4a）：前一条停在 `gate` 工具上（旧版用"结果不明"的 waiting，新存储没有那种停等），这样
/// `closing` 期间**手里还有活**；跑完之后由 reconcile / 后台补做推进到 `deleted`（判定，不是时钟）。
#[tokio::test]
async fn a_logically_deleted_session_refuses_new_input_and_names_the_state() {
    use crate::service::test_support::Gate;
    use crate::service::test_support::harness::{FakeLlm, Home, call_round, text_round};

    let home = Home::with_config(&config_toml(""));
    let gate = Gate::new();
    home.install_tools(vec![gate.tool()]);
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "life-1", "gate", serde_json::json!({})),
        text_round(2, "手里的活做完了。"),
    ]]);
    let gateway = home.start_durable(llm).await;
    let session = gateway.open_session().await;
    let first = gateway.submit(&session, "life-1", "先跑一条").await.run;
    gate.wait_entered(1).await;

    // `closing`：不再收新活，但列表里还在（§8.10 的列表那一列）。
    let (code, body) = gateway
        .post_json(
            &format!("/v1/sessions/{session}/delete"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["state"], "closing", "{body}");
    let (code, body) = gateway
        .post_json(
            &format!("/v1/sessions/{session}/runs"),
            serde_json::json!({ "request_key": "life-2", "text": "还能聊吗" }),
        )
        .await;
    assert_eq!(code, 409, "{body}");
    assert_eq!(body["error"]["code"], "conflict", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("closing"),
        "{body}"
    );

    let (code, list) = gateway.get_json("/v1/sessions").await;
    assert_eq!(code, 200);
    let visible: Vec<komo_kernel::protocol::http::SessionSummary> =
        serde_json::from_value(list["sessions"].clone()).expect("sessions");
    assert!(
        visible.iter().any(|one| one.session == session),
        "`closing` 要列出来并标注：{visible:?}"
    );

    // `closing` 仍然服务（§8.10）：手里那条 Run 照常跑完。
    gate.release();
    assert_eq!(
        gateway.wait_terminal(&first).await.summary.state,
        RunState::Completed,
        "closing 只是不收新活，手里的活要跑完"
    );

    // 活做完了：reconcile（或后台补做，谁先到）把 `closing` 推进到 `deleted`。
    let (code, body) = gateway
        .post_json("/v1/reconcile", serde_json::json!({}))
        .await;
    assert_eq!(code, 200, "{body}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (_, body) = gateway.get_json(&format!("/v1/sessions/{session}")).await;
        if body["state"] == "deleted" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "没有进 deleted：{body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // `deleted`：再删一次是幂等的；不再收新活，默认列表不再列，`?all=1` 才列（§8.10）。
    let (code, body) = gateway
        .post_json(
            &format!("/v1/sessions/{session}/delete"),
            serde_json::json!({ "now": true }),
        )
        .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["state"], "deleted", "{body}");
    let (code, body) = gateway
        .post_json(
            &format!("/v1/sessions/{session}/runs"),
            serde_json::json!({ "request_key": "life-3", "text": "现在呢" }),
        )
        .await;
    assert_eq!(code, 409, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("deleted"),
        "{body}"
    );

    let (_, list) = gateway.get_json("/v1/sessions").await;
    let default_list: Vec<komo_kernel::protocol::http::SessionSummary> =
        serde_json::from_value(list["sessions"].clone()).expect("sessions");
    assert!(
        default_list.iter().all(|one| one.session != session),
        "已逻辑删除的默认不列：{default_list:?}"
    );
    let (_, list) = gateway.get_json("/v1/sessions?all=1").await;
    let all: Vec<komo_kernel::protocol::http::SessionSummary> =
        serde_json::from_value(list["sessions"].clone()).expect("sessions");
    let shown = all
        .iter()
        .find(|one| one.session == session)
        .expect("`?all=1` 才列得出来");
    assert_eq!(
        shown.state,
        komo_kernel::types::status::SessionState::Deleted
    );
}

/// §8.10 第 3 条：**先算引用，再落墓碑，最后删内容**；幂等；`purged` 只剩墓碑。
///
/// Durable 版（W4a）：purge 只从 `deleted` 起步（没删就 409）；墓碑 + `PurgeConversations` 一次提交，之后删目录。
#[tokio::test]
async fn purging_a_session_removes_its_content_and_is_idempotent() {
    use crate::service::test_support::harness::{FakeLlm, Home, text_round};

    let home = Home::with_config(&config_toml(""));
    let llm = FakeLlm::always(vec![text_round(1, "好。")]);
    let gateway = home.start_durable(llm).await;
    let session = gateway.open_session().await;
    let run = gateway.submit(&session, "purge-1", "说句话").await.run;
    gateway.wait_terminal(&run).await;
    // 会话目录里有点东西（纯文字的一轮不会自己往里写）。
    let dir =
        komo_store::SessionPaths::new(&gateway.state().snapshot().paths.sessions_dir, &session);
    std::fs::create_dir_all(dir.root().join("artifacts")).unwrap();
    std::fs::write(dir.root().join("artifacts/note.txt"), b"some evidence").unwrap();

    // 没逻辑删除就 purge：409（新存储不替人做上一步）。
    let (code, body) = gateway
        .post_json(
            &format!("/v1/sessions/{session}/purge"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(code, 409, "{body}");

    let (code, body) = gateway
        .post_json(
            &format!("/v1/sessions/{session}/delete"),
            serde_json::json!({ "now": true }),
        )
        .await;
    assert_eq!(code, 200, "{body}");

    let (code, body) = gateway
        .post_json(
            &format!("/v1/sessions/{session}/purge"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["state"], "purged", "{body}");
    assert!(
        body["removed_bytes"].as_u64().unwrap_or(0) > 0,
        "真的删了内容才报得出来：{body}"
    );
    assert!(!dir.root().exists());

    // 幂等：再跑一次不报错，`removed_bytes` 是 0（目录已经不在）。
    let (code, body) = gateway
        .post_json(
            &format!("/v1/sessions/{session}/purge"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["removed_bytes"], 0, "{body}");

    // 墓碑还在答"它曾经存在"，但两种列表都不列它（§8.10）。
    let (code, _) = gateway.get_json(&format!("/v1/sessions/{session}")).await;
    assert_eq!(code, 200, "purged 的会话仍然答得出它曾经存在");
    let (_, list) = gateway.get_json("/v1/sessions?all=1").await;
    let all: Vec<komo_kernel::protocol::http::SessionSummary> =
        serde_json::from_value(list["sessions"].clone()).expect("sessions");
    assert!(all.iter().all(|one| one.session != session), "{all:?}");
}

/// §8.10 + §8.5：目录已经删了的 `purged` 会话，它在 outbox 里的审计行**不补写、标成已交付**，
/// 不再每拍报"已提交范围缺失"、也不挡着别的会话补写。
///
/// 复现的是 2026-10-15 实测的那种：`purge` 排完 `session.purged` 就删目录，补写器跑在删之后，
/// 那行就永远留在 outbox 里。这里在 purge 之后直接排一行，等于补写器输掉了那场竞态。**中间
/// 要重启一次**：同一代进程里会话的写入器还拿着已删文件的句柄，补写会"成功"写进一个已经
/// unlink 的 inode；卡死是在下一代——重新打开时文件没了、数据库却说写到过 seq N。
#[tokio::test]
async fn audit_rows_of_a_purged_session_are_discarded_and_do_not_block_others() {
    use crate::service::test_support::harness::{FakeLlm, Home, text_round};

    let home = Home::with_config(&config_toml(""));
    let llm = FakeLlm::always(vec![text_round(1, "好。")]);
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let gone = gateway.open_session().await;
    let run = gateway.submit(&gone, "purge-3", "说句话").await.run;
    gateway.wait_terminal(&run).await;
    let live = gateway.open_session().await;

    let (code, body) = gateway
        .post_json(
            &format!("/v1/sessions/{gone}/delete"),
            serde_json::json!({ "now": true }),
        )
        .await;
    assert_eq!(code, 200, "{body}");
    let (code, body) = gateway
        .post_json(&format!("/v1/sessions/{gone}/purge"), serde_json::json!({}))
        .await;
    assert_eq!(code, 200, "{body}");
    gateway.stop().await;
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let state = gateway.state();
    let paths = komo_store::SessionPaths::new(&state.snapshot().paths.sessions_dir, &gone);
    assert!(!paths.root().exists(), "purge 之后目录该没了");

    let enqueue = |session: komo_kernel::types::ids::SessionId, kind: &'static str| {
        let db = state.db.clone();
        let at = state.clock.now();
        let event = komo_kernel::types::ids::EventId::new_at(at);
        let payload = komo_kernel::events::EventPayload::Unknown {
            event_type: kind.to_string(),
            raw: serde_json::json!({ "session": session.as_str() }),
        };
        let id = event.clone();
        async move {
            db.with_write_retry(move |ex| {
                let (session, event, payload) = (session.clone(), event.clone(), payload.clone());
                Box::pin(async move {
                    komo_store::repos::outbox::enqueue_in(ex, &session, &event, payload, at).await
                })
                    as komo_store::db::BoxFuture<'_, Result<(), komo_kernel::traits::StoreError>>
            })
            .await
            .expect("排进 outbox");
            id
        }
    };
    let stale = enqueue(gone.clone(), "session.purged").await;
    let fresh = enqueue(live.clone(), "session.note").await;

    // 后台补写器也可能已经抢先处理过：按终态断言，不按这一次的返回值。
    state.drain_audit().await;
    let pending = komo_store::repos::outbox::pending(&state.db, 1024)
        .await
        .expect("读 outbox");
    assert!(
        pending.iter().all(|row| row.event_id != stale),
        "purged 会话的行该标成已交付：{pending:?}"
    );
    assert!(
        pending.iter().all(|row| row.event_id != fresh),
        "别的会话照常补写：{pending:?}"
    );
    assert!(!paths.root().exists(), "丢弃不该把目录建回来");
    let live_log = komo_store::SessionPaths::new(&state.snapshot().paths.sessions_dir, &live);
    let text = std::fs::read_to_string(live_log.root().join("events.jsonl")).expect("读 JSONL");
    assert!(
        text.contains(fresh.as_str()),
        "活着的会话里该有这条审计事件"
    );
}

/// §8.10 第 3 条：**引用检查不过就 409 并列出要先处置什么**，不假装成功。
#[tokio::test]
async fn purging_a_session_with_unfinished_work_is_a_409_that_lists_the_blockers() {
    use crate::service::test_support::harness::{FakeLlm, Home, call_round};

    let home = Home::with_config(&config_toml(""));
    // 停在一次"结果不明"上（测试工具 `unsure`）：未完成、要人答。
    let unsure = crate::service::test_support::Unsure::new();
    home.install_tools(vec![unsure.tool()]);
    let llm = FakeLlm::new(vec![vec![call_round(
        1,
        "p-1",
        "unsure",
        serde_json::json!({}),
    )]]);
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gateway.open_session().await;
    let run = gateway
        .submit(&session, "purge-2", "跑一条等着人答")
        .await
        .run;
    gateway.wait_waiting(&run).await;

    let (code, body) = gateway
        .post_json(
            &format!("/v1/sessions/{session}/purge"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(code, 409, "{body}");
    assert_eq!(
        body["session"],
        serde_json::json!(session.to_string()),
        "{body}"
    );
    let blockers = body["blockers"].as_array().cloned().unwrap_or_default();
    assert!(
        blockers
            .iter()
            .any(|blocker| blocker["what"] == "unfinished_run"),
        "未完成的 Run 要先处置：{body}"
    );
    // 内容还在：414 之后什么都没被删。
    let paths =
        komo_store::SessionPaths::new(&gateway.state().snapshot().paths.sessions_dir, &session);
    assert!(paths.root().exists(), "拒绝回收之后目录要原样在");
}

// ---------------------------------------------------------------- 委派（§4）
//
// 同步委派 2026-10-16 删掉，生产目录里没有 `delegate`。这一组装上测试用的旧工具
// （`legacy_delegate`），验的是升级前的委派调用照删之前的路径收尾。

use crate::service::test_support::harness::{FakeLlm, call_round, text_round};
use crate::service::test_support::legacy_delegate;

/// 委派的脚本：父要一次委派 → 子代理一轮给出结果 → 父拿到结果之后收尾。
fn delegation_script(task: &str, child_text: &str) -> Arc<FakeLlm> {
    FakeLlm::new(vec![
        vec![call_round(
            1,
            "pc-1",
            komo_agent::DELEGATE_TOOL,
            serde_json::json!({
                "task": task,
                "output_schema": {
                    "type": "object",
                    "required": ["files"],
                    "additionalProperties": false,
                    "properties": { "files": { "type": "integer" } },
                },
            }),
        )],
        // 子代理的一条 Run：它的一轮就是它的全部——最后那条回复就是结果。
        vec![text_round(1, child_text)],
        // 父续跑：手里那条工具结果就是子代理的结果。
        vec![text_round(1, "子代理数出来是 3 个文件。")],
    ])
}

/// 委派端到端：任务变成同 Session 里的一条**子 Run**，它自己跑完，结果回到父那次调用的
/// 工具结果上，父接着说下去（§4）。
#[tokio::test]
async fn a_delegated_task_becomes_a_child_run_and_its_result_reaches_the_parent() {
    use crate::service::test_support::harness::{Home, home_config};

    const TASK: &str = "只报个数：workspace 下有几个文件，用 JSON 回答";
    const CHILD_TEXT: &str = r#"{"files": 3}"#;
    let llm = delegation_script(TASK, CHILD_TEXT);

    let home = Home::with_config(&home_config());
    home.install_tools(vec![legacy_delegate()]);
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gateway.open_session().await;
    let parent = gateway
        .submit(&session, "delegate-1", "帮我把文件数一遍")
        .await
        .run;

    let done = gateway.wait_terminal(&parent).await;
    assert_eq!(done.summary.state, RunState::Completed, "{done:?}");

    // 子 Run 就是同 Session 里的一条普通 Run，只是带着"谁派的"与那份契约。
    let runs = komo_store::repos::runs::list_for_session(&gateway.state().db, &session)
        .await
        .expect("读得出会话里的 Run");
    let child = runs
        .iter()
        .find(|row| row.parent.as_ref() == Some(&parent))
        .expect("该有一条子 Run");
    assert_eq!(child.state, RunState::Completed, "{child:?}");
    let spec = child.delegate.as_ref().expect("子 Run 要带受理时那份 spec");
    assert_eq!(spec.task, TASK, "任务正文跟着子 Run 走（重启后也读得到）");
    // 幂等键从"父 + 那次调用"派生：受理子 Run 用**我们自己的 call_id**（不是 provider 给的
    // 那个 id——它是模型写的，重试时可能被复用），所以"受理了但还没 start 就崩了"在续跑时
    // 重上一次会拿回同一条子 Run，不会多出一条。
    assert!(
        child
            .request_key
            .as_str()
            .starts_with(&format!("delegate:{parent}:")),
        "{}",
        child.request_key.as_str()
    );

    // 父拿到的是那次调用的**工具结果**，不是子代理的对话：它走**轮输入**回到父那一轮
    // （不是回放窗口里的消息，窗口按 Run 过滤，子代理的轮次进不来）。
    let inputs = llm.inputs.lock().expect("脚本模型");
    let delivered = inputs
        .iter()
        .find_map(|input| match input {
            komo_kernel::types::turn::RoundInput::ToolResults { results, .. } => results
                .iter()
                .find(|result| result.content.contains(child.run.as_str())),
            _ => None,
        })
        .expect("父那一轮要拿到子代理的结果");
    assert!(
        !delivered.is_error,
        "子代理正常结束，这条结果不该是错误：{delivered:?}"
    );
    assert_eq!(
        llm.requests.lock().expect("脚本模型").len(),
        3,
        "父一轮 + 子一轮 + 父续跑一轮"
    );
}

/// 委派的任务正文超过 §8.3 的 4 KiB 内联上限：**参数被外置**到 `payloads/`，父续跑时
/// 必须按引用读回来。
///
/// 读不回来会拿到一份空参数：重新 `prepare` 当场失败，而那条失败结论只存在于内存里
/// （`execute_one` 的 prepare 失败分支不写账本）——父这一轮拿到的就不是子 Run 的结果，
/// 而父的窗口里从此悬着一次**没有输出的 `function_call`**。
#[tokio::test]
async fn a_delegated_task_over_the_inline_limit_still_settles_the_parent_call() {
    use crate::service::test_support::harness::{Home, home_config};
    use komo_kernel::events::EventPayload;
    use komo_kernel::types::ids::Seq;

    // 中文按 3 字节算，这段正文远过 4 KiB。
    let task = format!(
        "只报个数：{}workspace 下有几个文件，用 JSON 回答",
        "背景说明，逐条列在这里。".repeat(400)
    );
    assert!(
        task.len() > 4096,
        "这条测试要真的踩到外置：{} 字节",
        task.len()
    );
    let llm = delegation_script(&task, r#"{"files": 3}"#);

    let home = Home::with_config(&home_config());
    home.install_tools(vec![legacy_delegate()]);
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gateway.open_session().await;
    let parent = gateway
        .submit(&session, "delegate-big", "帮我把文件数一遍")
        .await
        .run;
    let done = gateway.wait_terminal(&parent).await;
    assert_eq!(done.summary.state, RunState::Completed, "{done:?}");

    // 超限参数完整留在事件里，委派恢复仍沿用同一份计划。
    let paths = gateway.state().ledgers.paths_for(&session);
    let (events, _) = komo_store::session_log::read_events(&paths, &session, Seq::ZERO, 0)
        .await
        .expect("读得出会话");
    assert!(
        events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::MessageAssistant(body)
                if body.tool_calls.iter().any(|call| {
                    call.arguments_ref.is_none()
                        && call.arguments.to_string().contains("背景说明")
                })
        )),
        "超限参数应完整内联在 JSONL 事件里"
    );
    // §8.4 第 4 行：**沿用原计划**。续跑重新 `prepare` 会写出第二条 `tool.planned`，
    // 而它换了一个 `plan_hash`——原先那份授权就覆盖不到了（§7.4）。
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.run.as_ref() == Some(&parent)
                    && matches!(&event.payload, EventPayload::ToolPlanned(_))
            })
            .count(),
        1,
        "委派调用只该有一份计划：续跑要沿用已经落盘的那份"
    );

    let runs = komo_store::repos::runs::list_for_session(&gateway.state().db, &session)
        .await
        .expect("读得出会话里的 Run");
    let child = runs
        .iter()
        .find(|row| row.parent.as_ref() == Some(&parent))
        .expect("该有一条子 Run");
    let inputs = llm.inputs.lock().expect("脚本模型");
    let delivered = inputs
        .iter()
        .find_map(|input| match input {
            komo_kernel::types::turn::RoundInput::ToolResults { results, .. } => results
                .iter()
                .find(|result| result.content.contains(child.run.as_str())),
            _ => None,
        })
        .expect("父续跑要拿到子 Run 的结果，而不是一次 prepare 失败");
    assert!(
        !delivered.is_error,
        "子 Run 正常结束，这条结果不该是错误：{delivered:?}"
    );
}

/// 工具名不认识：那条结论**要落盘**——它没有执行过，但它有结论。
///
/// 只把错误交给模型、不写账本的话，这次调用在账本上永远悬着，而任何从账本重建的转写都
/// 带着一个没有输出的 `function_call`——provider 直接 400（`No tool output found for
/// tool call …`），一次打错工具名的调用就能把整个会话后面的每一段毒住。
#[tokio::test]
async fn a_call_that_cannot_run_still_gets_a_result_in_the_ledger() {
    use crate::service::test_support::harness::{
        FakeLlm, Home, call_round, home_config, text_round,
    };
    use komo_kernel::events::EventPayload;
    use komo_kernel::types::ids::Seq;
    use komo_kernel::types::status::ToolCallState;

    let llm = FakeLlm::new(vec![
        vec![call_round(
            1,
            "pc-1",
            "rm_rf",
            serde_json::json!({ "path": "/" }),
        )],
        vec![text_round(1, "没有这个工具，我换个办法。")],
    ]);

    let home = Home::with_config(&home_config());
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gateway.open_session().await;
    let run = gateway
        .submit(&session, "unknown-tool", "把根目录删了")
        .await
        .run;
    let done = gateway.wait_terminal(&run).await;
    assert_eq!(done.summary.state, RunState::Completed, "{done:?}");

    let paths = gateway.state().ledgers.paths_for(&session);
    let (events, _) = komo_store::session_log::read_events(&paths, &session, Seq::ZERO, 0)
        .await
        .expect("读得出会话");
    let surface = komo_kernel::fold::fold(&events);
    let call = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::MessageAssistant(body) => {
                body.tool_calls.first().map(|call| call.call_id.clone())
            }
            _ => None,
        })
        .expect("那一轮提了一次调用");

    // ① 那次调用有终态，账本上没有悬着的东西。
    assert_eq!(
        surface.calls.get(&call).map(|view| view.state),
        Some(ToolCallState::Failed),
        "没执行过的调用也要有结论"
    );
    assert!(
        surface.open_calls(&run).is_empty(),
        "账本上不许有没收口的调用"
    );

    // ② 模型看到的那句话就在账本里（预览是它的摘要，正文在 tool-output 里）。
    let preview = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolResult(body) if body.call_id == call => body.preview.clone(),
            _ => None,
        })
        .expect("要有一条 tool.result");
    // 拦住它的是**能力面**（`rm_rf` 不在这次运行交给模型的那份 schema 里），所以话是
    // "这次运行的工具集里没有它"——执行器一次都不回退到全局工具目录（§4）。
    assert!(preview.contains("工具集里没有 rm_rf"), "{preview}");
    assert!(preview.contains("可用的是："), "{preview}");

    // ③ 它确实**没有执行过**：没有计划，也没有 `tool.started`。
    assert!(!events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolPlanned(body) if body.call_id == call
    )));
    assert!(!events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolStarted(body) if body.call_id == call
    )));
}

/// 子代理只拿得到任务：不继承父的对话历史、不注入记忆，也**不再拿到 `delegate`**
/// （深度只有一层）；反过来，它跑过的那几轮也不许跑进父的窗口（§4）。
#[tokio::test]
async fn a_child_gets_only_its_task_and_its_rounds_stay_out_of_the_parent() {
    use crate::service::test_support::harness::{Home, home_config};

    const TASK: &str = "只报个数：workspace 下有几个文件，用 JSON 回答";
    const PARENT_INPUT: &str = "帮我把文件数一遍";
    let llm = delegation_script(TASK, r#"{"files": 3}"#);

    let home = Home::with_config(&home_config());
    home.install_tools(vec![legacy_delegate()]);
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gateway.open_session().await;
    let parent = gateway
        .submit(&session, "delegate-1", PARENT_INPUT)
        .await
        .run;
    gateway.wait_terminal(&parent).await;

    let requests = llm.requests.lock().expect("脚本模型");
    assert_eq!(requests.len(), 3, "父一轮 + 子一轮 + 父续跑一轮");

    // ① 父那一轮：有 `delegate` 工具，窗口里只有用户那句话。
    let parent_first = &requests[0];
    assert!(
        parent_first
            .tools
            .iter()
            .any(|tool| tool.name == komo_agent::DELEGATE_TOOL),
        "父要看得见 delegate"
    );
    assert_eq!(
        parent_first.messages.len(),
        1,
        "{:?}",
        parent_first.messages
    );

    // ② 子那一轮：提示里是任务，不是父的输入；窗口里只有那条任务；没有 `delegate`。
    let child = &requests[1];
    assert!(
        child.system_prompt.contains(TASK),
        "{}",
        child.system_prompt
    );
    assert!(
        !child.system_prompt.contains(PARENT_INPUT),
        "父的输入不该出现在子代理的提示里：{}",
        child.system_prompt
    );
    assert_eq!(
        child.messages.len(),
        1,
        "子代理只带任务：{:?}",
        child.messages
    );
    assert_eq!(child.messages[0].text.as_deref(), Some(TASK));
    assert!(
        !child
            .tools
            .iter()
            .any(|tool| tool.name == komo_agent::DELEGATE_TOOL),
        "深度只有一层：子代理不该再看见 delegate"
    );

    // ③ 父续跑那一轮：子代理跑过的那几轮不在父的窗口里（结果只从工具结果进来）。
    let parent_again = &requests[2];
    assert!(
        !parent_again
            .messages
            .iter()
            .any(|message| message.text.as_deref() == Some(r#"{"files": 3}"#)),
        "子代理的回复不该作为消息进父的窗口：{:?}",
        parent_again.messages
    );
}

/// 取消父 Run 时，它派出去、还没结束的子 Run 一并被取消（§4）：操作者撤的是这件事。
#[tokio::test]
async fn cancelling_the_parent_cancels_its_child_run() {
    use crate::service::test_support::harness::{FakeLlm, Home, config_toml};
    use komo_kernel::types::delegate::DelegateSpec;
    use komo_kernel::types::ids::{RequestKey, ToolCallId};
    use komo_kernel::types::plan::PlanSource;
    use komo_kernel::types::status::RunEnd;

    let home = Home::with_config(&config_toml(""));
    let gateway = home.start(FakeLlm::finisher("好。")).await;
    let session = gateway.open_session().await;
    let state = gateway.state();
    let now = state.clock.now();
    let parent = RunId::new_at(now);
    let child = RunId::new_at(now);

    // 两条行照 §8.5 的顺序落下来：父是普通 Run，子带着"谁派的"。
    let seed = |run: RunId, key: &str, delegate: Option<DelegateSpec>| {
        let new = komo_store::repos::runs::NewRun {
            run,
            session: session.clone(),
            request_key: RequestKey::new(key),
            input_hash: "seed".into(),
            source: PlanSource::Interactive {
                session: session.clone(),
            },
            peer: None,
            model: state.snapshot().model.clone(),
            effort: None,
            delegate,
            skip_memory: false,
            at: now,
        };
        let db = state.db.clone();
        async move {
            db.with_write_retry(move |ex| {
                let new = new.clone();
                Box::pin(async move {
                    komo_store::repos::runs::reserve_in(ex, &new)
                        .await
                        .map(|_| ())
                })
                    as komo_store::db::BoxFuture<'_, Result<(), komo_kernel::traits::StoreError>>
            })
            .await
            .expect("落一条 Run");
        }
    };
    seed(parent.clone(), "p", None).await;
    seed(
        child.clone(),
        "c",
        Some(DelegateSpec::new(
            parent.clone(),
            ToolCallId::from_raw("pc-1"),
            "数文件",
        )),
    )
    .await;

    gateway
        .state()
        .finish_run(&session, &parent, RunEnd::Cancelled { by: None })
        .await
        .expect("取消父");

    let runs = komo_store::repos::runs::list_for_session(&gateway.state().db, &session)
        .await
        .expect("读得出会话里的 Run");
    let child_row = runs
        .iter()
        .find(|row| row.run == child)
        .expect("子 Run 还在账本里");
    assert_eq!(
        child_row.state,
        RunState::Cancelled,
        "父被取消，子代理不能继续跑：{child_row:?}"
    );
}

/// 注册用的工具名与工具自己报的名字必须一致——「两处各写一个字面量」迟早会漂。目录里
/// 有 `task`，没有 2026-10-16 删掉的 `delegate` / `dispatch` / `follow`。
#[tokio::test]
async fn the_catalog_has_task_and_none_of_the_retired_names() {
    use crate::service::test_support::harness::{Home, home_config};

    let home = Home::with_config(&home_config());
    let gateway = home.start(FakeLlm::finisher("好。")).await;
    let names = gateway.state().tool_names();
    assert!(
        names.iter().any(|name| name == komo_agent::TASK_TOOL),
        "{names:?}"
    );
    for retired in ["delegate", "dispatch", "follow"] {
        assert!(!names.iter().any(|name| name == retired), "{names:?}");
    }
    gateway.stop().await;
}

// ---------------------------------------------------------------- TUI 的 `!`
/// **`!` 不经模型直接跑，结果记进会话**：同一会话连跑两条，两条都真的执行（不会把前
/// 一条的结果认成这一条的），全程零模型请求；下一轮模型的回放窗口里看得见两条输出。
#[tokio::test]
async fn a_bang_command_runs_without_the_model_and_lands_in_the_history() {
    use crate::service::test_support::harness::{FakeLlm, Home, text_round};

    let home = Home::new();
    let count = home.workspace().join("bang.count");
    let llm = FakeLlm::new(vec![vec![text_round(1, "看到了。")]]);
    let gateway = home.start_durable(Arc::clone(&llm)).await;
    let session = gateway.open_session().await;

    for (key, word) in [("bang-1", "first-out"), ("bang-2", "second-out")] {
        let (status, body) = gateway
            .post_json(
                &format!("/v1/sessions/{session}/runs"),
                serde_json::json!({
                    "request_key": key,
                    "text": format!("echo {word}; echo x >> {}", count.display()),
                    "shell": true,
                }),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let run = RunId::from_raw(body["run"].as_str().expect("run").to_string());
        let detail = gateway.wait_terminal(&run).await;
        assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
        assert!(
            detail
                .final_message
                .as_deref()
                .is_some_and(|text| text.contains(word)),
            "最终答复就是命令输出：{detail:?}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&count)
            .unwrap_or_default()
            .lines()
            .count(),
        2,
        "两条命令都要真的跑过"
    );
    assert_eq!(
        llm.turns(),
        0,
        "`!` 全程不发模型请求（起标题、记忆提取也不发）"
    );

    let run = gateway
        .submit(&session, "after-bang", "刚才输出了什么")
        .await
        .run;
    gateway.wait_terminal(&run).await;
    let requests = llm.requests.lock().expect("脚本模型");
    let seen = format!("{:?}", requests[0].messages);
    assert!(
        seen.contains("first-out") && seen.contains("second-out"),
        "下一轮模型要看得见 `!` 的输出：{seen}"
    );
}

/// `!` 后面没有命令：说清楚怎么用，不受理。
#[tokio::test]
async fn an_empty_bang_command_is_refused() {
    use crate::service::test_support::harness::{FakeLlm, Home};

    let home = Home::new();
    let gateway = home.start_durable(FakeLlm::new(vec![])).await;
    let session = gateway.open_session().await;
    let (status, body) = gateway
        .post(
            &format!("/v1/sessions/{session}/runs"),
            serde_json::json!({ "request_key": "bang-empty", "text": "  ", "shell": true }),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("!ls"), "{body}");
}

/// 受理 Run 时把探到的窗口补进模型快照；配置里写了的不被盖掉；没探到的保持 `None`。
#[tokio::test]
async fn acceptance_fills_the_probed_window_only_when_config_has_none() {
    use crate::service::test_support::harness::{FakeLlm, Home, text_round};
    use crate::service::windows::{WindowState, key_of};

    let home = Home::new();
    let gateway = home
        .start(FakeLlm::new(vec![
            vec![text_round(1, "一")],
            vec![text_round(1, "二")],
            vec![text_round(1, "三")],
        ]) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let state = gateway.state();
    let model = state.snapshot().model.clone();
    assert_eq!(model.context_window, None);

    let window_of = |run: RunId| {
        let db = state.db.clone();
        async move {
            komo_store::repos::runs::get(&db, &run)
                .await
                .expect("读得到")
                .expect("有这个 Run")
                .model
                .context_window
        }
    };

    // 没探到 / 探到但 provider 没说：None。
    let session = gateway.open_session().await;
    let run = gateway.submit(&session, "w-1", "一").await.run;
    assert_eq!(window_of(run).await, None);
    state.windows.finish(key_of(&model), WindowState::Missing);
    let run = gateway.submit(&session, "w-2", "二").await.run;
    assert_eq!(window_of(run).await, None);

    // 探到了：补进快照。
    state
        .windows
        .finish(key_of(&model), WindowState::Found(500_000));
    let run = gateway.submit(&session, "w-3", "三").await.run;
    assert_eq!(window_of(run).await, Some(500_000));
}

/// `/v1/config/check` 按 alias 列出窗口来源：配置写了的是 config，探到的是 provider，其余 unknown。
#[tokio::test]
async fn config_check_lists_window_sources_per_alias() {
    use crate::service::windows::{WindowState, key_of};
    use komo_kernel::protocol::http::{ConfigCheckResponse, WindowSource};

    let gateway = TestGateway::with(
        &config_toml(
            r#"
[model.job]
type = "completion"
api_backend = "responses"
base_url = "https://jobs.example.com/v1"
model = "job-model"
api_key_env = "KOMO_LLM_API_KEY"
context_window = 64000
"#,
        ),
        Some(crate::service::test_support::harness::FakeLlm::new(vec![])
            as Arc<dyn komo_kernel::traits::LlmClient>),
    )
    .await;
    let main = gateway.state().snapshot().model.clone();
    let (status, body) = gateway.get("/v1/config/check").await;
    assert_eq!(status, 200, "{body}");
    let response: ConfigCheckResponse = serde_json::from_str(&body).expect("能解码");
    let source = |alias: &str| {
        response
            .model_windows
            .iter()
            .find(|w| w.alias == alias)
            .unwrap_or_else(|| panic!("缺 {alias}"))
            .source
    };
    assert_eq!(source("job"), WindowSource::Config);
    assert_eq!(source("main"), WindowSource::Unknown);

    gateway
        .state()
        .windows
        .finish(key_of(&main), WindowState::Found(500_000));
    let (_, body) = gateway.get("/v1/config/check").await;
    let response: ConfigCheckResponse = serde_json::from_str(&body).expect("能解码");
    let main_row = response
        .model_windows
        .iter()
        .find(|w| w.alias == "main")
        .unwrap();
    assert_eq!(
        (main_row.source, main_row.window),
        (WindowSource::Provider, Some(500_000))
    );
}

// ---------------------------------------------------------------- run inspect

fn usage_round(
    n: u32,
    text: &str,
    usage: komo_kernel::types::model::TokenUsage,
) -> Result<komo_kernel::types::turn::Round, komo_kernel::types::turn::LlmError> {
    Ok(komo_kernel::types::turn::Round {
        usage,
        ..crate::service::test_support::harness::text_round(n, text).unwrap()
    })
}

/// `GET /v1/runs/{id}`：终态 Run 带结束时刻、耗时、模型与各轮用量之和。
#[tokio::test]
async fn a_completed_run_reports_end_time_duration_model_and_token_totals() {
    use crate::service::test_support::harness::{FakeLlm, Home, call_round};
    use komo_kernel::types::model::TokenUsage;

    let home = Home::new();
    let gateway = home
        .start_durable(FakeLlm::new(vec![vec![
            call_round(1, "pc-1", "read", serde_json::json!({"path": "nope"})),
            usage_round(
                2,
                "好了。",
                TokenUsage {
                    input: Some(50),
                    output: Some(5),
                    cache_read: Some(30),
                    ..TokenUsage::default()
                },
            ),
        ]]))
        .await;
    let session = gateway.open_session().await;
    let run = gateway.submit(&session, "ins-1", "看一下").await.run;
    gateway
        .wait_db_state(&run, RunState::is_terminal, "终态")
        .await;

    let detail = gateway.run_detail(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed);
    let ended = detail.summary.ended_at.expect("终态带结束时刻");
    assert!(ended >= detail.summary.created_at);
    assert!(detail.duration_ms.is_some());
    assert!(detail.model.is_some(), "受理快照里的模型");
    assert!(detail.failure.is_none());
    let tokens = detail.tokens.expect("有一轮报了用量");
    assert_eq!(tokens.input, Some(50));
    assert_eq!(tokens.output, Some(5));
    assert_eq!(tokens.cache_read, Some(30));
    assert_eq!(tokens.cache_write, None, "没人报的项保持未知，不是 0");
}

/// 一轮都没报过用量：`tokens` 整个缺席，而不是一排 0。
#[tokio::test]
async fn a_run_without_reported_usage_leaves_token_totals_absent() {
    use crate::service::test_support::harness::{FakeLlm, Home, text_round};

    let home = Home::new();
    let gateway = home
        .start_durable(FakeLlm::new(vec![vec![text_round(1, "好。")]]))
        .await;
    let session = gateway.open_session().await;
    let run = gateway.submit(&session, "ins-2", "嗯").await.run;
    gateway
        .wait_db_state(&run, RunState::is_terminal, "终态")
        .await;

    let (status, body) = gateway.get(&format!("/v1/runs/{run}")).await;
    assert_eq!(status, 200, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(json.get("tokens").is_none(), "{body}");
    assert!(json.get("failure").is_none(), "{body}");
}

/// 失败的 Run 带上失败原因。
#[tokio::test]
async fn a_failed_run_reports_why() {
    use crate::service::test_support::harness::{FakeLlm, Home};
    use komo_kernel::types::turn::LlmError;

    let home = Home::new();
    let gateway = home
        .start_durable(FakeLlm::always(vec![Err(LlmError::Rejected {
            status: 400,
            message: "额度用完了".into(),
        })]))
        .await;
    let session = gateway.open_session().await;
    let run = gateway.submit(&session, "ins-3", "来").await.run;
    gateway
        .wait_db_state(&run, RunState::is_terminal, "终态")
        .await;

    let detail = gateway.run_detail(&run).await;
    assert_eq!(detail.summary.state, RunState::Failed);
    assert!(detail.summary.ended_at.is_some());
    let reason = detail.failure.expect("失败原因");
    assert!(reason.contains("额度用完了"), "{reason}");
}

/// codemode 脚本读得到会话日志（与 `read` 一致），数据库、`runtime/` 与 `.env` 仍读不到；
/// write / edit 的名单不变，`sessions/` 照旧改不了。
#[test]
fn codemode_scripts_may_read_session_logs_but_not_the_rest_of_komo_state() {
    use crate::service::state::{codemode_unreadable_paths, protected_paths};
    let snapshot = crate::service::test_support::sample_snapshot();
    let home = std::path::Path::new("/tmp/komo");
    let unreadable = codemode_unreadable_paths(&snapshot, home);
    assert!(!unreadable.contains(&snapshot.paths.sessions_dir));
    assert!(unreadable.contains(&snapshot.start_only.db_path));
    assert!(unreadable.contains(&snapshot.paths.runtime_dir));
    assert!(unreadable.contains(&home.join(".env")));
    assert!(protected_paths(&snapshot, home).contains(&snapshot.paths.sessions_dir));
}

// ---------------------------------------------------------------- W2b：Durable 版

/// 起一台 Durable Gateway 并接上 SSE feed（`start()` 里的接线由主控合并时补；`spawn` 幂等）。
async fn durable_gateway_with_feed(
    llm: Arc<crate::service::test_support::harness::FakeLlm>,
) -> (TestGateway, komo_kernel::traits::Shutdown) {
    let gateway = TestGateway::durable(&telegram_config("111"), llm).await;
    let shutdown = komo_kernel::traits::Shutdown::new();
    let state = gateway.state();
    let _ =
        crate::service::durable::feed::spawn(state.core().unwrap(), &state.hub, &shutdown).await;
    (gateway, shutdown)
}

/// ⑧ Durable 版：SSE 从 `Last-Event-ID` 续读，且先历史后直播。
///
/// 会话与输入直接走 `DurableCore`（HTTP 的会话路由是 W2a 的）；事件路由是 W2b 的，照旧走 HTTP。
#[tokio::test]
async fn sse_replays_history_from_a_cursor_then_goes_live_durable() {
    use crate::service::durable::sessions::NewSession;
    use crate::service::durable::submit::InputRequest;
    use crate::service::test_support::harness::{FakeLlm, text_round};
    use futures_util::StreamExt;

    let (gateway, shutdown) =
        durable_gateway_with_feed(FakeLlm::new(vec![vec![text_round(1, "收到")]])).await;
    let core = gateway.state().core().unwrap();
    let session = core
        .create_session(NewSession::new("assistant"))
        .await
        .unwrap()
        .id;
    let accepted = core
        .submit_input(&session, InputRequest::new("第一条").request_id("k-1"))
        .await
        .unwrap();
    gateway.wait_run(&accepted.run).await;

    // 游标 = 这条 run 的 `run.accepted` 那一帧的号：之后从 `run.queued` 起补读，游标那条不再给。
    let (status, body) = gateway
        .get(&format!("/v1/sessions/{session}/events?from=0"))
        .await;
    assert_eq!(status, 200, "{body}");
    let page: komo_kernel::protocol::http::EventPage = serde_json::from_str(&body).unwrap();
    let cursor = page
        .events
        .iter()
        .find(|event| {
            matches!(
                event.payload,
                komo_kernel::events::EventPayload::RunAccepted(_)
            )
        })
        .expect("有 run.accepted")
        .seq;

    let stream = reqwest::Client::new()
        .get(format!(
            "{}/v1/sessions/{session}/events",
            gateway.base_url()
        ))
        .bearer_auth(gateway.token())
        .header("accept", "text/event-stream")
        .header("last-event-id", cursor.0.to_string())
        .send()
        .await
        .expect("连得上");
    assert_eq!(stream.status(), 200);

    let mut body = stream.bytes_stream();
    let mut seen = String::new();
    while let Ok(Some(chunk)) =
        tokio::time::timeout(std::time::Duration::from_secs(5), body.next()).await
    {
        seen.push_str(&String::from_utf8_lossy(&chunk.expect("一块")));
        if seen.contains("run.completed") {
            break;
        }
    }
    assert!(seen.contains("run.queued"), "游标之后才补读：{seen}");
    assert!(
        !seen.contains(&format!("id: {}\n", cursor.0)),
        "游标那一条不再给一遍：{seen}"
    );

    // 直播：现在写一条新的，同一条连接上就能收到。
    core.boundary(&session, None).await.expect("写得下");
    let mut live = String::new();
    while let Ok(Some(chunk)) =
        tokio::time::timeout(std::time::Duration::from_secs(5), body.next()).await
    {
        live.push_str(&String::from_utf8_lossy(&chunk.expect("一块")));
        if live.contains("conversation.boundary") {
            break;
        }
    }
    assert!(
        live.contains("conversation.boundary"),
        "补读之后接直播：{live}"
    );
    let (_, body) = gateway
        .get(&format!("/v1/sessions/{session}/events?from={}", cursor.0))
        .await;
    let page: komo_kernel::protocol::http::EventPage = serde_json::from_str(&body).unwrap();
    let boundary = page
        .events
        .iter()
        .find(|event| {
            matches!(
                event.payload,
                komo_kernel::events::EventPayload::ConversationBoundary(_)
            )
        })
        .expect("补读也有这条 boundary");
    assert!(
        live.contains(&format!("id: {}\n", boundary.seq.0)),
        "直播的帧号与补读的一致：{live}"
    );
    shutdown.cancel();
}

/// ⑨ Durable 版：提交输入 → Run 完成 → 事件流上有最终消息。
#[tokio::test]
async fn an_end_to_end_turn_completes_and_reaches_the_event_stream_durable() {
    use crate::service::durable::sessions::NewSession;
    use crate::service::durable::submit::InputRequest;
    use crate::service::test_support::harness::{FakeLlm, text_round};

    let (gateway, shutdown) = durable_gateway_with_feed(FakeLlm::new(vec![vec![text_round(
        1,
        "好了，两加二等于四。",
    )]]))
    .await;
    let core = gateway.state().core().unwrap();
    let session = core
        .create_session(NewSession::new("assistant"))
        .await
        .unwrap()
        .id;
    let accepted = core
        .submit_input(
            &session,
            InputRequest::new("二加二等于几").request_id("cli-1"),
        )
        .await
        .unwrap();
    let settled = gateway.wait_run(&accepted.run).await;
    assert_eq!(
        settled.status(),
        komo_kernel::durable::SubmissionStatus::Done
    );

    // 事件流上有那条 assistant 消息与最终消息（补读，不依赖有没有人在线）。
    let (status, body) = gateway
        .get(&format!("/v1/sessions/{session}/events?from=0"))
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("message.assistant"), "{body}");
    assert!(body.contains("两加二等于四"), "{body}");
    let page: komo_kernel::protocol::http::EventPage = serde_json::from_str(&body).unwrap();
    let completed = page
        .events
        .iter()
        .find_map(|event| match &event.payload {
            komo_kernel::events::EventPayload::RunCompleted(done) => Some(done.clone()),
            _ => None,
        })
        .expect("有 run.completed");
    assert_eq!(
        completed.final_message.as_deref(),
        Some("好了，两加二等于四。")
    );
    assert_eq!(
        page.events.last().map(|event| event.seq),
        Some(page.next),
        "next 是最后一帧的号"
    );
    shutdown.cancel();
}

// ---------------------------------------------------------------- 聊天渠道（Durable 存储，D5 W3a）
//
// 下面五个与上面 Legacy 的同名测试各是一对：同一句话、同一个意图，Gateway 跑在新存储上。读账本的
// 地方从 `runs` 表 / JSONL 换成 sessions 行、submission 与 entry；与 Legacy 不同的断言写在各自的注释里。

mod durable_chat {
    use super::*;
    use crate::service::test_support::harness::{
        FakeLlm, FixedFactory, GatewayBuilder, MemSender, TestGateway as Harness, durable_inputs_of,
    };

    /// 起一台跑在新存储上的 Gateway：Telegram 渠道（操作者 111）+ 内存发送口。
    async fn start(llm: Arc<FakeLlm>) -> (crate::service::test_support::TestGateway, Arc<FakeLlm>) {
        let gateway = crate::service::test_support::TestGateway::durable(
            &telegram_config("111"),
            Arc::clone(&llm),
        )
        .await;
        (gateway, llm)
    }

    /// ① 同一个 `request_key` 重发只产生一个 Run。
    #[tokio::test]
    async fn a_replayed_message_produces_one_run() {
        let (gateway, _) = start(FakeLlm::finisher("好")).await;
        let first = gateway
            .dispatcher()
            .handle(operator_dm("帮我看看", "telegram:42"))
            .await
            .expect("第一条");
        let InboundAck::Queued { session, run } = first else {
            panic!("第一条应该排队：{first:?}");
        };

        // 平台重投：同一个 update_id 再来一次。
        let again = gateway
            .dispatcher()
            .handle(operator_dm("帮我看看", "telegram:42"))
            .await
            .expect("重投");
        assert_eq!(
            again,
            InboundAck::Duplicate {
                run: Some(run.clone())
            },
            "重投不该开第二个 Run"
        );

        // 改了断言的读法：Legacy 数 `runs` 表，新存储里一条 run 就是它的首个输入 submission。
        assert_eq!(
            durable_inputs_of(gateway.state(), session.as_str()).await,
            1,
            "账本上只有一个 Run"
        );
    }

    /// 聊天渠道的普通消息以插话受理（§6，2026-10-23）：宿主 Run 忙着跑工具时，同一个 peer 的
    /// 下一句话由它在下一次请求模型之前并进去，不排到整条 Run 后面。
    ///
    /// 改了断言：Legacy 看 `run.accepted` 上的 `interject` 标记；新存储的 RunAccepted 是合成的，
    /// 没有那一格，插话体现为行为——宿主停在 `gate` 上时第二句话并进它（终态 `absorbed`），
    /// 渠道收到"已并入"，回答只有宿主给的一次。
    #[tokio::test]
    async fn a_plain_chat_message_is_accepted_as_an_interjection() {
        use crate::service::test_support::Gate;
        let sender = MemSender::new(ChannelPlatform::Telegram);
        let gate = Gate::new();
        let gateway = GatewayBuilder::new(&telegram_config("111"))
            .durable()
            .fake_llm(FakeLlm::new(vec![vec![
                call_round(1, "pc-1", "gate", serde_json::json!({})),
                text_round(2, "好了。"),
            ]]))
            .tool(gate.tool())
            .factory(FixedFactory::sender_only(
                Arc::clone(&sender) as Arc<dyn crate::channels::ChannelSender>
            ))
            .start()
            .await;
        let ack = gateway.handle(operator_dm("稍等", "telegram:50")).await;
        let InboundAck::Queued { run: host, .. } = ack else {
            panic!("应该排队：{ack:?}");
        };
        gate.wait_entered(1).await;
        let ack = gateway.handle(operator_dm("补一句", "telegram:51")).await;
        let InboundAck::Queued { run: second, .. } = ack else {
            panic!("应该排队：{ack:?}");
        };

        gate.release();
        gateway.wait_run(&host).await;
        gateway.wait_run(&second).await;
        let (code, body) = gateway.get(&format!("/v1/runs/{second}")).await;
        assert_eq!(code, 200, "{body}");
        let detail: komo_kernel::protocol::http::RunDetail = serde_json::from_str(&body).unwrap();
        assert_eq!(
            detail.summary.state,
            RunState::Absorbed,
            "渠道的普通消息带插话：并进了宿主，没有另开一条"
        );
        let told = sender.sent().iter().any(|message| {
            matches!(&message.outbound, Outbound::RunAbsorbed { run, into, .. }
                if run == &second && into == &host)
        });
        assert!(told, "渠道要收到并入通知：{:?}", sender.sent());
    }

    /// 渠道带来的话题上下文：普通文本把它拼进提交的正文（引用块 + "发给你的消息："），
    /// 命令只解析原文、不受影响（§11.3 话题上下文）。
    ///
    /// 改了断言的读法：正文从输入 entry（`chat.input`）读，不再从 JSONL 的 `run.accepted`；"命令没有
    /// 起第二个 Run"数的是 submission。
    #[tokio::test]
    async fn thread_context_is_prepended_to_plain_text_but_never_to_commands() {
        let (gateway, _) = start(FakeLlm::finisher("好")).await;

        let mut plain = operator_dm("看看 owl 的两条 Blocking", "telegram:60");
        plain.context = Some("张三 10:23：B1 在 Drop，B2 在 escape".into());
        let ack = gateway.dispatcher().handle(plain).await.expect("处理了");
        let InboundAck::Queued { session, run } = ack else {
            panic!("应该排队：{ack:?}");
        };
        gateway.wait_run(&run).await;
        let tree = gateway
            .state()
            .core()
            .unwrap()
            .read_tree(session.as_str())
            .await
            .expect("读得出树");
        let text = tree
            .branches
            .iter()
            .flat_map(|branch| branch.entries.iter())
            .filter(|entry| entry.kind == "chat.input")
            .find_map(crate::service::durable::tree::input_text)
            .expect("这条输入放置过");
        assert!(text.starts_with("> 【话题上下文】"), "{text}");
        assert!(
            text.contains("> 张三 10:23：B1 在 Drop，B2 在 escape\n"),
            "{text}"
        );
        assert!(
            text.ends_with("发给你的消息：\n看看 owl 的两条 Blocking"),
            "{text}"
        );

        // 命令：照旧按原文解析，上下文被丢掉，不起 Run。
        let mut status = operator_dm("/status", "telegram:61");
        status.context = Some("张三 10:23：这段不该影响命令".into());
        let ack = gateway.dispatcher().handle(status).await.expect("处理了");
        assert!(matches!(ack, InboundAck::Replied { .. }), "{ack:?}");
        assert_eq!(
            durable_inputs_of(gateway.state(), session.as_str()).await,
            1,
            "命令没有起第二个 Run"
        );
    }

    /// ② 不在 `allow_from` 的发送者被拒、`/id` 对他可用、且**会话与 deliveries 无任何记录**。
    #[tokio::test]
    async fn a_stranger_is_refused_and_leaves_no_trace() {
        let (gateway, _) = start(FakeLlm::finisher("好")).await;
        let stranger = inbound(
            ChannelPlatform::Telegram,
            "999",
            "999",
            "在吗",
            "telegram:1",
            true,
        );

        let ack = gateway.dispatcher().handle(stranger).await.expect("处理了");
        let InboundAck::Rejected { hint } = ack else {
            panic!("不在名单里的应该被拒：{ack:?}");
        };
        assert!(hint.contains("999"), "{hint}");
        assert!(hint.contains("allow_from"), "{hint}");

        // `/id` 对任何人可用——这是唯一不要求操作者身份的命令。
        let id = gateway
            .dispatcher()
            .handle(inbound(
                ChannelPlatform::Telegram,
                "999",
                "999",
                "/id",
                "telegram:2",
                true,
            ))
            .await
            .expect("处理了");
        assert!(
            matches!(&id, InboundAck::Replied { text } if text.contains("telegram:999")),
            "{id:?}"
        );

        // 不留任何记录：没有会话、没有投递。
        let sessions = gateway
            .state()
            .core()
            .unwrap()
            .list_sessions(true)
            .await
            .expect("读得到");
        assert!(
            sessions.is_empty(),
            "被拒绝的消息不该建出会话：{sessions:?}"
        );
        let pending = gateway
            .state()
            .notifier
            .log()
            .pending(None)
            .await
            .expect("读得到");
        assert!(pending.is_empty(), "也不该留下投递记录：{pending:?}");
    }

    /// ⑥ `Deferred` 的投递在下一条入站消息时先被冲刷。
    #[tokio::test]
    async fn a_deferred_delivery_is_flushed_before_the_next_message() {
        let (gateway, _) = start(FakeLlm::finisher("好")).await;
        // 微信那条路径：此刻没有回复令牌。
        gateway.channel.defer(true);

        let peer = ChannelPeer::new(ChannelPlatform::Telegram, "111");
        let delivery = gateway
            .state()
            .notifier
            .deliver(
                &DeliveryTarget::to_peer(peer.clone()),
                Outbound::Text {
                    text: "要批一下".into(),
                },
            )
            .await;
        use komo_kernel::traits::Notifier;
        let delivery = delivery.expect("登记得上");
        assert_eq!(
            delivery.state,
            komo_kernel::types::chat::DeliveryState::Deferred
        );
        assert!(
            gateway.channel.sent().is_empty(),
            "推不出去就没有送出这一条"
        );

        // 用户发了消息，令牌有了。
        gateway.channel.defer(false);
        gateway
            .dispatcher()
            .handle(operator_dm("在", "telegram:9"))
            .await
            .expect("处理了");

        let sent = gateway.channel.sent();
        assert!(
            sent.iter().any(
                |message| matches!(&message.outbound, Outbound::Text { text } if text == "要批一下")
            ),
            "积压的那一条要先送到：{sent:?}"
        );
        assert!(
            gateway
                .state()
                .notifier
                .log()
                .pending(None)
                .await
                .unwrap()
                .is_empty(),
            "冲刷之后不再 pending"
        );
        let _: Option<Harness> = None;
    }
}
