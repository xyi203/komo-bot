//! 验收（§14 第 4 阶段那一行，以及 §3 的热重载、§11.4 的投递、§13.1 的 SSE）。
//!
//! 每个测试断言的是**行为**，不是"状态变成了什么"：一条消息重发只产生一个 Run、
//! 一条被拒绝的消息在账本与投递表里都查不到、非法配置重载之后旧名单仍然生效。

use std::sync::Arc;

use komo_kernel::protocol::{InboundAck, InboundMessage};
use komo_kernel::traits::Inbound;
use komo_kernel::types::chat::{
    ApprovalScope, ChannelPeer, ChannelPlatform, DeliveryTarget, Outbound, PeerId,
};
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

/// ① 的另一半：重发的 `/approve` 只批准一次。
#[tokio::test]
async fn a_replayed_approve_decides_once() {
    let gateway = TestGateway::start().await;
    let record = pending_approval(&gateway).await;

    let first = gateway
        .dispatcher()
        .handle(operator_dm(
            &format!("/approve {}", record.short_id),
            "telegram:7",
        ))
        .await
        .expect("第一次");
    let InboundAck::Replied { text } = first else {
        panic!("命令要有回执：{first:?}");
    };
    assert!(text.contains("已批准"), "{text}");
    assert!(!text.contains("已经决定过"), "第一次不是重复：{text}");

    // 平台重投同一条命令。
    let again = gateway
        .dispatcher()
        .handle(operator_dm(
            &format!("/approve {}", record.short_id),
            "telegram:7",
        ))
        .await
        .expect("重投");
    assert!(
        matches!(&again, InboundAck::Replied { text } if text.contains("已批准")),
        "{again:?}"
    );

    let stored = gateway
        .state()
        .approval_repo
        .get(&record.approval)
        .await
        .expect("读得到")
        .expect("有这条");
    let decision = stored.decision.expect("有结论");
    assert!(decision.approved);
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

/// ⑤ 审批请求投到来源会话与 home chat；第二个答复得到"已决定"。
#[tokio::test]
async fn an_approval_goes_to_both_the_source_and_home_and_is_decided_once() {
    // home chat 是 111，来源会话是群 222。
    let gateway = TestGateway::start().await;
    let record = pending_approval(&gateway).await;

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

    let sent = gateway.channel.sent();
    let targets: Vec<String> = sent
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

    // 第二个答复得到"已决定"，且不改变结论。
    let second = gateway
        .state()
        .decide_approval(&record.approval, false, ApprovalScope::Once, None)
        .await
        .expect("决定得了");
    assert!(second.already_decided, "第二个答复应当得到原决定");
    assert!(second.decision.approved, "结论没有被第二次答复改写");
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
        "积压的审批请求要先送到：{sent:?}"
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
        llm: None,
        embeddings: None,
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
    let gateway = TestGateway::start().await;

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
    let gateway = TestGateway::start().await;
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
#[tokio::test]
async fn slash_new_appends_a_boundary_to_the_same_session() {
    let gateway = TestGateway::start().await;
    let first = gateway
        .dispatcher()
        .handle(operator_dm("在", "telegram:1"))
        .await
        .unwrap();
    let InboundAck::Queued { session, .. } = first else {
        panic!("{first:?}");
    };

    let ack = gateway
        .dispatcher()
        .handle(operator_dm("/new", "telegram:2"))
        .await
        .unwrap();
    assert!(matches!(ack, InboundAck::Replied { .. }), "{ack:?}");

    // 还是同一个 home session。
    assert_eq!(gateway.state().home_session().await.unwrap(), session);
}

// ---------------------------------------------------------------- 工具

/// 造一条待处理的审批（executor 在真的跑起来时做的就是这件事）。
async fn pending_approval(gateway: &TestGateway) -> komo_kernel::protocol::http::ApprovalRecord {
    let session = gateway.state().home_session().await.expect("home session");
    let plan = komo_kernel::test_support::sample_plan("shell", &session);
    gateway
        .state()
        .approvals
        .request(komo_runtime::approvals::ApprovalRequest {
            session,
            run: None,
            call: None,
            plan,
            reason: "任意 shell 命令要人看一眼".into(),
            changes: None,
            evidence: None,
            scopes: vec![ApprovalScope::Once],
        })
        .await
        .expect("写得下")
}

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

/// 待处理只有一条时，聊天里回一个 `y` 就批了——**不用抄短 ID**（§11.3）。
///
/// 而且没有待处理审批时它**不是命令**：模型问"要不要…"、操作者回个 `n`，那是回话，不是
/// "拒绝一条不存在的审批"。
#[tokio::test]
async fn a_bare_yes_decides_the_only_pending_approval() {
    let gateway = TestGateway::start().await;
    let record = pending_approval(&gateway).await;

    let ack = gateway
        .dispatcher()
        .handle(operator_dm("y", "telegram:900"))
        .await
        .expect("答复");
    assert!(matches!(ack, InboundAck::Replied { .. }), "{ack:?}");

    let decided = gateway
        .state()
        .approval_repo
        .get(&record.approval)
        .await
        .expect("读得到")
        .expect("有这条");
    assert!(
        decided.decision.as_ref().is_some_and(|d| d.approved),
        "`y` 要真的批了：{decided:?}"
    );
}

/// 没有待处理审批时，`y` / `n` 走普通消息那条路（不是命令）。
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
        "没有待审批时 `n` 是回话，不该被读成拒绝：{ack:?}"
    );
}

// ---------------------------------------------------------------- §3 的 policy 热重载

/// **改 `policy.toml` 不用重启**——改完下一次判决就用新表。
///
/// 这条测试盯的是一个真实缺陷：executor 的 `PolicyEngine` 曾是装配时造的，热重载只换了
/// 快照，判决仍旧按旧表走。于是 `mode = "auto"` 换上去、日志也说"配置已重载"，可每一次
/// 调用照样问人（线上就卡在这里）。
#[tokio::test]
async fn a_reloaded_policy_decides_the_next_call() {
    use crate::service::test_support::harness::{FakeLlm, Home, call_round, text_round};

    // 一开始不写 policy.toml：§7.1 的初始建议生效，shell 要问。
    let home = Home::new();
    let once = home.workspace().join("reload.count");
    let command = format!("echo once >> {}", once.display());

    let first = home
        .start(FakeLlm::new(vec![vec![
            call_round(
                1,
                "pc-1",
                "shell",
                serde_json::json!({ "command": command }),
            ),
            text_round(2, "做完了。"),
        ]]) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = first.open_session().await;
    let run = first.submit(&session, "reload-1", "跑一下").await.run;
    let approval = first.wait_approval().await;
    assert_eq!(approval.run.as_ref(), Some(&run), "初始建议下这条要问");
    first.decide(&approval.approval, false).await;
    first.stop().await;

    // 换成 auto（**不重启**），再来一条同样的命令。
    std::fs::write(home.path().join("policy.toml"), "mode = \"auto\"\n").expect("写 policy.toml");
    let second = home
        .start(FakeLlm::new(vec![vec![
            call_round(
                1,
                "pc-1",
                "shell",
                serde_json::json!({ "command": command }),
            ),
            text_round(2, "做完了。"),
        ]]) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    // 这条是**重启之后**的启动路径，先证明它按新表装了；再证明热重载这条路也一样。
    let session = second.open_session().await;
    let run = second.submit(&session, "reload-2", "跑一下").await.run;
    second
        .wait_state(
            &run,
            |status| status.is_terminal() || status == RunState::Waiting,
            "收场",
        )
        .await;
    assert!(
        second
            .approvals()
            .await
            .iter()
            .all(|record| record.run.as_ref() != Some(&run)),
        "auto 表下这条不该问：{:?}",
        second.approvals().await
    );
}

/// 同一台实例上：**热重载**（不重启）之后，判决立刻按新表走。
#[tokio::test]
async fn hot_reloading_the_policy_changes_the_very_next_decision() {
    use crate::service::test_support::harness::{
        FakeLlm, Home, call_round, home_config, text_round,
    };

    // 起点：auto —— 命令不问就跑。
    let home = Home::with_policy(&home_config(), "mode = \"auto\"\n");
    let gateway = home
        .start(FakeLlm::new(vec![vec![
            call_round(
                1,
                "pc-1",
                "shell",
                serde_json::json!({ "command": "echo hi" }),
            ),
            text_round(2, "做完了。"),
        ]]) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;

    // 换成 strict：**同一台实例**，不重启，只重载。
    std::fs::write(
        home.path().join("policy.toml"),
        "default = \"ask\"\n\n[[rules]]\nid = \"ask-shell\"\neffect = \"ask\"\nreason = \"重载之后这条要问我\"\nscopes = [\"once\"]\nrequires_isolation = false\n\n[rules.matcher]\noperations = [\"shell_command\"]\n",
    )
    .expect("写 policy.toml");
    crate::reload::reload(gateway.state())
        .await
        .expect("新配置装得上");

    let session = gateway.open_session().await;
    let run = gateway.submit(&session, "reload-3", "跑一下").await.run;
    let approval = gateway.wait_approval().await;
    assert_eq!(
        approval.run.as_ref(),
        Some(&run),
        "重载之后必须按新表问：{:?}",
        approval.reason
    );
    assert!(
        approval.reason.contains("重载之后这条要问我"),
        "理由要来自新表：{}",
        approval.reason
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
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
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

// ---------------------------------------------------------------- §7.1 的 auto 模式
/// `policy.toml` 写 `mode = "auto"`：**日常命令不问就跑**（§7.1）。
///
/// 这是这个模式存在的全部理由——`mode = "strict"`（以及不写 `policy.toml` 时的初始建议）
/// 对每一条 shell 都问一次，于是 agent 干任何活都要人去点一下。
#[tokio::test]
async fn auto_mode_runs_an_ordinary_shell_command_without_asking() {
    use crate::service::test_support::harness::{
        FakeLlm, Home, call_round, home_config, text_round,
    };

    let home = Home::with_policy(&home_config(), "mode = \"auto\"\n");
    let once = home.workspace().join("auto.count");
    let command = format!("echo once >> {}", once.display());
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-1",
            "shell",
            serde_json::json!({ "command": command }),
        ),
        text_round(2, "做完了。"),
    ]]);
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gateway.open_session().await;
    let run = gateway.submit(&session, "auto-1", "跑一下 echo").await.run;
    gateway
        .wait_state(
            &run,
            |status| status.is_terminal() || status == RunState::Waiting,
            "收场",
        )
        .await;

    assert!(
        gateway.approvals().await.is_empty(),
        "auto 模式下这条命令不该问：{:?}",
        gateway.approvals().await
    );
    assert_eq!(
        std::fs::read_to_string(&once)
            .unwrap_or_default()
            .lines()
            .count(),
        1,
        "命令要真的跑过"
    );
}

/// 同一个文件里，**危险形状也不问**：auto 是"不审批"，不是"少问几条"。
///
/// 靶子是一条不存在的路径（`rm -rf <workspace>/never`）：即使放行也删不掉别的东西，
/// 但可以断言"一条审批都没产生"且"命令真的执行了"——先建文件，再看它没了。
#[tokio::test]
async fn auto_mode_runs_even_a_dangerous_command_without_asking() {
    use crate::service::test_support::harness::{
        FakeLlm, Home, call_round, home_config, text_round,
    };

    let home = Home::with_policy(&home_config(), "mode = \"auto\"\n");
    let doom = home.workspace().join("never-touched");
    let command = format!("rm -rf {}", doom.display());
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-1",
            "shell",
            serde_json::json!({ "command": command }),
        ),
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
    gateway
        .wait_state(
            &run,
            |status| status.is_terminal() || status == RunState::Waiting,
            "收场",
        )
        .await;

    assert!(
        gateway.approvals().await.is_empty(),
        "auto 模式一条都不该问：{:?}",
        gateway.approvals().await
    );
    assert!(!doom.exists(), "命令要真的执行过");
}

// ---------------------------------------------------------------- §8.4 / §7.5 / §8.10

/// §7.5 第 2 条 / §8.4：**等前一条 Run 的那一类不进清单，但说得出在等谁。**
///
/// 「挡着会话的每一条都必须在清单里」说的是**停在人身上**的那两类（`approval`、
/// `intervention`）。`dependency` 在等前一条 Run 跑完，那件事不需要人回答——把它塞进清单
/// 等于让操作者去答一个他答不了的问题；反过来把它藏起来（旧模型里的"排队中"），就正好是
/// §8.4 要消掉的那个"排队二十分钟不知道在等什么"。所以它只在这两格露面：
/// `RunSummary.wait` 与 `SessionSummary.current_wait`。
#[tokio::test]
async fn a_dependency_wait_is_not_an_intervention_but_says_which_run_it_waits_for() {
    use crate::service::test_support::harness::{FakeLlm, Home, call_round};

    let home = Home::with_config(&config_toml(""));
    // 前一条 Run 停在审批上——**确定性**地占住队列；批复之后两段脚本都用默认的收尾。
    let llm = FakeLlm::new(vec![vec![call_round(
        1,
        "a-1",
        "shell",
        serde_json::json!({ "command": "echo 一" }),
    )]]);
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gateway.open_session().await;
    let first = gateway.submit(&session, "dep-1", "先跑这个").await.run;
    let first_wait = gateway.wait_waiting(&first).await;
    assert!(
        matches!(first_wait, WaitReason::Approval { .. }),
        "前一条该停在审批上：{first_wait:?}"
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
        "这个会话上停着的是前一条那条审批：{pending:?}"
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
    let record = gateway.wait_approval().await;
    gateway.decide(&record.approval, true).await;
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
#[tokio::test]
async fn a_logically_deleted_session_refuses_new_input_and_names_the_state() {
    use crate::service::test_support::harness::{FakeLlm, Home, call_round};

    let home = Home::with_config(&config_toml(""));
    // 前一条停在审批上：这样 `closing` 期间**手里还有活**——§8.10 说 `closing` 仍然服务，
    // 而未完成 Run 在的时候对账不会把它推进到 `deleted`（那一步是判定，不是时钟）。
    let llm = FakeLlm::new(vec![vec![call_round(
        1,
        "life-1",
        "shell",
        serde_json::json!({ "command": "echo 一" }),
    )]]);
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gateway.open_session().await;
    let first = gateway.submit(&session, "life-1", "先跑一条").await.run;
    gateway.wait_waiting(&first).await;

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

    // `closing` 仍然服务（§8.10）：手里那条 Run 照常答复、跑完。
    let record = gateway.wait_approval().await;
    gateway.decide(&record.approval, true).await;
    assert_eq!(
        gateway.wait_terminal(&first).await.summary.state,
        RunState::Completed,
        "closing 只是不收新活，手里的活要跑完"
    );

    // `deleted`：默认列表不再列，`?all=1` 才列（§8.10）。
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
#[tokio::test]
async fn purging_a_session_removes_its_content_and_is_idempotent() {
    use crate::service::test_support::harness::{FakeLlm, Home, text_round};

    let home = Home::with_config(&config_toml(""));
    let llm = FakeLlm::always(vec![text_round(1, "好。")]);
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gateway.open_session().await;
    let run = gateway.submit(&session, "purge-1", "说句话").await.run;
    gateway.wait_terminal(&run).await;

    // 未完成的 Run 挡着就先 409 并列出要先处置什么——那一条由下一个测试管；这里先把会话
    // 逻辑删除掉（`purge` 只从 `deleted` 起步）。
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

/// §8.10 第 3 条：**引用检查不过就 409 并列出要先处置什么**，不假装成功。
#[tokio::test]
async fn purging_a_session_with_unfinished_work_is_a_409_that_lists_the_blockers() {
    use crate::service::test_support::harness::{FakeLlm, Home, call_round};

    let home = Home::with_config(&config_toml(""));
    let llm = FakeLlm::new(vec![vec![call_round(
        1,
        "p-1",
        "shell",
        serde_json::json!({ "command": "echo 一" }),
    )]]);
    let gateway = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gateway.open_session().await;
    let run = gateway
        .submit(&session, "purge-2", "跑一条等着批")
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
