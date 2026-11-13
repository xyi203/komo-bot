//! 验收（§14 的 Gateway 行为，以及 §3 的热重载、§13.1 的 SSE）。
//!
//! 每个测试断言的是**行为**，不是"状态变成了什么"：一条消息重发只产生一个 Run、
//! 非法配置重载之后旧配置仍然生效。

use std::sync::Arc;

use komo_kernel::types::ids::{RequestKey, RunId};
use komo_kernel::types::status::RunState;

use crate::service::test_support::{TestGateway, config_toml};

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

/// ④ 非法配置重载失败：旧配置继续生效。
#[tokio::test]
async fn an_invalid_reload_keeps_the_old_config() {
    let gateway = TestGateway::start().await;
    let before = gateway.state().snapshot().model.model.clone();

    // `listen` 不是"地址:端口"——校验里的一个 Error。
    gateway.write_config(&config_toml(
        r#"
[gateway]
listen = "这不是一个地址"
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
}

/// ⑦ 多个进程同时启动只有一个拿到锁：第二台 Gateway 起不来。
#[tokio::test]
async fn only_one_gateway_holds_a_data_directory() {
    let gateway = TestGateway::start().await;
    let second = crate::service::start(crate::service::ServiceOptions {
        home: Some(gateway.home.path().to_path_buf()),
        listen: Some("127.0.0.1:0".into()),
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

/// 认证：除 `/healthz` 外统一 Bearer。
#[tokio::test]
async fn everything_but_health_needs_the_token() {
    let gateway = TestGateway::with(
        &config_toml(""),
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
    let gateway = TestGateway::with(
        &config_toml(""),
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

// ---------------------------------------------------------------- 工具

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
        ]]))
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
    let gateway = home.start(Arc::clone(&llm)).await;
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
    let gateway = home.start(Arc::clone(&llm)).await;
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
    let gateway = home.start(Arc::clone(&llm)).await;
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
    let gateway = home.start(Arc::clone(&llm)).await;
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
    let gateway = home.start(llm).await;
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
    let gateway = home.start(llm).await;
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

// ---------------------------------------------------------------- TUI 的 `!`
/// **`!` 不经模型直接跑，结果记进会话**：同一会话连跑两条，两条都真的执行（不会把前
/// 一条的结果认成这一条的），全程零模型请求；下一轮模型的回放窗口里看得见两条输出。
#[tokio::test]
async fn a_bang_command_runs_without_the_model_and_lands_in_the_history() {
    use crate::service::test_support::harness::{FakeLlm, Home, text_round};

    let home = Home::new();
    let count = home.workspace().join("bang.count");
    let llm = FakeLlm::new(vec![vec![text_round(1, "看到了。")]]);
    let gateway = home.start(Arc::clone(&llm)).await;
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
    let gateway = home.start(FakeLlm::new(vec![])).await;
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
        crate::service::test_support::harness::FakeLlm::new(vec![]),
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
        .start(FakeLlm::new(vec![vec![
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
        .start(FakeLlm::new(vec![vec![text_round(1, "好。")]]))
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
        .start(FakeLlm::always(vec![Err(LlmError::Rejected {
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

/// 被删掉的东西（`delegate` / `dispatch` / `follow` 工具、审批式的 `y`）没有踪影。
///
/// - `the_catalog_has_task_and_none_of_the_retired_names`：读的是 `tool_names()`（`ToolExecutor` 目录）；
///   任务会话里再去掉 `task` / `monitor` 是钩子的事（`hooks/tests.rs`）；
/// - `a_bare_yes_with_nothing_pending_is_just_a_message`：`y` / `n` 走普通消息那条路；"有介入挂着时 `y`
///   也只是一句话"由 intervention 只认 `abandon` 保证（`durable::interventions` 的测试）。
mod retired {
    use super::*;
    use crate::service::test_support::harness::FakeLlm;

    #[tokio::test]
    async fn the_catalog_has_task_and_none_of_the_retired_names() {
        let gateway = TestGateway::with(&config_toml(""), FakeLlm::finisher("好。")).await;
        let names = gateway.state().tool_names();
        assert!(
            names.iter().any(|name| name == komo_agent::TASK_TOOL),
            "{names:?}"
        );
        for retired in ["delegate", "dispatch", "follow"] {
            assert!(!names.iter().any(|name| name == retired), "{names:?}");
        }
    }

    #[tokio::test]
    async fn a_bare_yes_with_nothing_pending_is_just_a_message() {
        let gateway = TestGateway::with(&config_toml(""), FakeLlm::finisher("好。")).await;
        let (_, body) = gateway.post("/v1/sessions", serde_json::json!({})).await;
        let session: komo_kernel::protocol::http::SessionSummary =
            serde_json::from_str(&body).unwrap();
        let (code, body) = gateway
            .post(
                &format!("/v1/sessions/{}/runs", session.session),
                serde_json::json!({ "request_key": "n-901", "text": "n" }),
            )
            .await;
        assert_eq!(code, 200, "`n` 是回话，不该被读成拒绝：{body}");
        let receipt: komo_kernel::protocol::http::SubmitRunResponse =
            serde_json::from_str(&body).unwrap();
        let record = gateway.wait_run(&receipt.run).await;
        assert_eq!(
            crate::service::durable::sessions::runs::state_of_submission(&record),
            RunState::Completed,
            "它跑了一轮模型：{record:?}"
        );
    }
}

// ---------------------------------------------------------------- SSE

/// 起一台 Gateway 并接上 SSE feed（`spawn` 对同一个 core 幂等）。
async fn gateway_with_feed(
    llm: Arc<crate::service::test_support::harness::FakeLlm>,
) -> (TestGateway, komo_kernel::traits::Shutdown) {
    let gateway = TestGateway::with(&config_toml(""), llm).await;
    let shutdown = komo_kernel::traits::Shutdown::new();
    let state = gateway.state();
    let _ =
        crate::service::durable::feed::spawn(state.core().unwrap(), &state.hub, &shutdown).await;
    (gateway, shutdown)
}

/// ⑧ SSE 从 `Last-Event-ID` 续读，且先历史后直播。
///
/// 会话与输入直接走 `DurableCore`（HTTP 的会话路由是 W2a 的）；事件路由是 W2b 的，照旧走 HTTP。
#[tokio::test]
async fn sse_replays_history_from_a_cursor_then_goes_live() {
    use crate::service::durable::sessions::NewSession;
    use crate::service::durable::submit::InputRequest;
    use crate::service::test_support::harness::{FakeLlm, text_round};
    use futures_util::StreamExt;

    let (gateway, shutdown) =
        gateway_with_feed(FakeLlm::new(vec![vec![text_round(1, "收到")]])).await;
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

/// ⑨ 提交输入 → Run 完成 → 事件流上有最终消息。
#[tokio::test]
async fn an_end_to_end_turn_completes_and_reaches_the_event_stream() {
    use crate::service::durable::sessions::NewSession;
    use crate::service::durable::submit::InputRequest;
    use crate::service::test_support::harness::{FakeLlm, text_round};

    let (gateway, shutdown) = gateway_with_feed(FakeLlm::new(vec![vec![text_round(
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
    // `run.accepted` / `run.queued` 有真实时刻（以前是 1970）：取 submission 的提交时刻，
    // 与随后的帧差不多同时。
    let first_started = page
        .events
        .iter()
        .find(|event| event.type_name() == "run.started")
        .map(|event| event.ts)
        .expect("有 run.started");
    for name in ["run.accepted", "run.queued"] {
        let at = page
            .events
            .iter()
            .find(|event| event.type_name() == name)
            .map(|event| event.ts)
            .unwrap_or_else(|| panic!("有 {name}"));
        assert!(
            (first_started - at).abs() < time::Duration::seconds(30),
            "{name} 的时间 {at} 离 run.started（{first_started}）太远"
        );
    }
    shutdown.cancel();
}

// ---------------------------------------------------------------- 提交
//
// 消息经 `POST /v1/sessions/{id}/runs` 落到 sessions 行、submission 与 entry（D5 W3a）。

mod submit {
    use super::*;
    use crate::service::test_support::harness::{
        FakeLlm, GatewayBuilder, TestGateway as Harness, call_round, inputs_of, text_round,
    };

    /// ① 同一个 `request_key` 重发只产生一个 Run。
    #[tokio::test]
    async fn a_replayed_message_produces_one_run() {
        let gateway = Harness::start().await;
        let (session, run) = gateway.say("帮我看看", "key-42").await;

        // 重投：同一个 request_key 再来一次。
        let again = gateway.say_in(&session, "帮我看看", "key-42", false).await;
        assert!(again.deduplicated, "重投不该开第二个 Run");
        assert_eq!(again.run, run, "返回的是原来那个 Run");

        // 一条 run 就是它的首个输入 submission：账本上只有一个。
        assert_eq!(
            inputs_of(gateway.state(), session.as_str()).await,
            1,
            "账本上只有一个 Run"
        );
    }

    /// 插话（§6）：宿主 Run 忙着跑工具时，带 `interject` 的下一句话由它在下一次请求模型之前
    /// 并进去，不排到整条 Run 后面。
    ///
    /// 改了断言：Legacy 看 `run.accepted` 上的 `interject` 标记；新存储的 RunAccepted 是合成的，
    /// 没有那一格，插话体现为行为——宿主停在 `gate` 上时第二句话并进它（终态 `absorbed`）。
    #[tokio::test]
    async fn an_interjected_message_is_absorbed_by_the_running_run() {
        use crate::service::test_support::Gate;
        let gate = Gate::new();
        let gateway = GatewayBuilder::new(&config_toml(""))
            .fake_llm(FakeLlm::new(vec![vec![
                call_round(1, "pc-1", "gate", serde_json::json!({})),
                text_round(2, "好了。"),
            ]]))
            .tool(gate.tool())
            .start()
            .await;
        let (session, host) = gateway.say("稍等", "key-50").await;
        gate.wait_entered(1).await;
        let second = gateway.say_in(&session, "补一句", "key-51", true).await.run;

        gate.release();
        gateway.wait_run(&host).await;
        gateway.wait_run(&second).await;
        let (code, body) = gateway.get(&format!("/v1/runs/{second}")).await;
        assert_eq!(code, 200, "{body}");
        let detail: komo_kernel::protocol::http::RunDetail = serde_json::from_str(&body).unwrap();
        assert_eq!(
            detail.summary.state,
            RunState::Absorbed,
            "插话并进了宿主，没有另开一条"
        );
    }
}

/// 从 Web 改模型配置（§3）：读 → 建 provider（key 只进 .env）→ 挂模型 → 改默认，每一步都
/// 热装上；旧版本 409、校验不过 422 且文件不动、还在用的不让删。
#[tokio::test]
async fn model_settings_are_edited_through_the_gateway_and_reloaded() {
    use komo_kernel::protocol::http::{ConfigEditResponse, ConfigModelsResponse};

    let gateway = TestGateway::configured(&config_toml("")).await;
    let home = gateway.state().home.clone();
    let config_file = || std::fs::read_to_string(home.join("config.toml")).unwrap();

    let (status, body) = gateway.get("/v1/config/models").await;
    assert_eq!(status, 200, "{body}");
    let view: ConfigModelsResponse = serde_json::from_str(&body).unwrap();
    assert!(view.providers.is_empty());
    assert_eq!(view.default.as_deref(), Some("main"));
    assert!(
        view.models
            .iter()
            .any(|m| m.alias == "main" && m.standalone)
    );
    let v1 = view.version;

    let edit = |body: &str| -> ConfigEditResponse { serde_json::from_str(body).expect(body) };
    let (status, body) = gateway
        .request(
            reqwest::Method::PUT,
            "/v1/config/model-providers/proxy",
            Some(serde_json::json!({
                "version": v1, "base_url": "https://proxy.example.com/v1",
                "api_backend": "responses", "api_key": "sk-web-secret"
            })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("sk-web-secret"), "响应里没有凭证值：{body}");
    let after = edit(&body).models;
    let proxy = after.providers.iter().find(|p| p.id == "proxy").unwrap();
    assert_eq!(proxy.env_key.as_deref(), Some("PROXY_API_KEY"));
    assert!(proxy.key_set);
    let env = std::fs::read_to_string(home.join(".env")).unwrap();
    assert!(env.contains("PROXY_API_KEY='sk-web-secret'"), "{env}");
    assert!(
        env.contains("KOMO_LLM_API_KEY=test-key"),
        "别的行不动：{env}"
    );
    assert!(!config_file().contains("sk-web-secret"));

    // 拿着旧版本再写：409，什么都不动。
    let (status, body) = gateway
        .request(
            reqwest::Method::PUT,
            "/v1/config/models/fast",
            Some(serde_json::json!({ "version": v1, "provider": "proxy", "model": "m-fast" })),
        )
        .await;
    assert_eq!(status, 409, "{body}");

    let (status, body) = gateway
        .request(
            reqwest::Method::PUT,
            "/v1/config/models/fast",
            Some(serde_json::json!({
                "version": after.version, "provider": "proxy", "model": "m-fast", "name": "Fast"
            })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let after = edit(&body).models;
    let (_, menu) = gateway.get("/v1/models").await;
    assert!(menu.contains("\"fast\""), "新模型热装上了：{menu}");

    // 校验不过：422，文件一个字节不动。
    let before = config_file();
    let (status, body) = gateway
        .request(
            reqwest::Method::PUT,
            "/v1/config/models/fast",
            Some(serde_json::json!({
                "version": after.version, "provider": "proxy", "model": "m-fast", "effort": "ultra"
            })),
        )
        .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(config_file(), before);

    let (status, body) = gateway
        .request(
            reqwest::Method::PUT,
            "/v1/config/default-model",
            Some(serde_json::json!({ "version": after.version, "alias": "fast" })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let after = edit(&body).models;
    assert_eq!(gateway.state().snapshot().model_catalog.default, "fast");

    // 还在用的不让删。
    let (status, body) = gateway
        .request(
            reqwest::Method::DELETE,
            "/v1/config/model-providers/proxy",
            Some(serde_json::json!({ "version": after.version })),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    let (status, body) = gateway
        .request(
            reqwest::Method::DELETE,
            "/v1/config/models/fast",
            Some(serde_json::json!({ "version": after.version })),
        )
        .await;
    assert_eq!(status, 409, "默认模型不让删：{body}");
}

/// 常用设置与环境变量：改一项热装上、白名单外 404；变量只写 `.env`，列表与响应里没有任何值，
/// 还被引用的删不掉。
#[tokio::test]
async fn settings_and_env_vars_are_edited_through_the_gateway_without_values_leaking() {
    use komo_kernel::protocol::http::{
        ConfigEditResponse, ConfigEnvResponse, ConfigSettingsResponse,
    };

    let gateway = TestGateway::configured(&config_toml("")).await;
    let home = gateway.state().home.clone();
    let env_file = || std::fs::read_to_string(home.join(".env")).unwrap();
    let edit = |body: &str| -> ConfigEditResponse { serde_json::from_str(body).expect(body) };

    let (status, body) = gateway.get("/v1/config/settings").await;
    assert_eq!(status, 200, "{body}");
    let view: ConfigSettingsResponse = serde_json::from_str(&body).unwrap();
    let compaction = view
        .settings
        .iter()
        .find(|s| s.key == "compaction.enabled")
        .unwrap();
    let was = compaction.effective.clone().unwrap().as_bool().unwrap();

    let (status, body) = gateway
        .request(
            reqwest::Method::PUT,
            "/v1/config/settings/compaction.enabled",
            Some(serde_json::json!({ "version": view.version, "value": !was })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let after = edit(&body);
    assert_eq!(
        gateway.state().snapshot().compaction.enabled,
        !was,
        "热装上了"
    );
    let shown = after
        .settings
        .settings
        .iter()
        .find(|s| s.key == "compaction.enabled")
        .unwrap();
    assert_eq!(shown.effective, Some(serde_json::Value::Bool(!was)));
    assert_eq!(
        after.env.version, after.settings.version,
        "三份视图共用一个版本"
    );

    let (status, body) = gateway
        .request(
            reqwest::Method::PUT,
            "/v1/config/settings/gateway.listen",
            Some(serde_json::json!({ "version": after.settings.version, "value": "0.0.0.0:1" })),
        )
        .await;
    assert_eq!(status, 404, "白名单之外：{body}");

    let (status, body) = gateway
        .request(
            reqwest::Method::PUT,
            "/v1/config/env",
            Some(serde_json::json!({
                "version": after.env.version,
                "vars": [{ "name": "WEB_TOKEN", "value": "tok-web-secret" }]
            })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("tok-web-secret"), "响应里没有值：{body}");
    let after = edit(&body);
    assert!(env_file().contains("WEB_TOKEN='tok-web-secret'"));
    let (_, listed) = gateway.get("/v1/config/env").await;
    assert!(
        !listed.contains("tok-web-secret") && !listed.contains("test-key"),
        "{listed}"
    );
    let listed: ConfigEnvResponse = serde_json::from_str(&listed).unwrap();
    let token = listed.vars.iter().find(|v| v.name == "WEB_TOKEN").unwrap();
    assert!(token.in_file && token.set && token.used_by.is_empty());
    let llm = listed
        .vars
        .iter()
        .find(|v| v.name == "KOMO_LLM_API_KEY")
        .unwrap();
    assert!(!llm.used_by.is_empty(), "模型用着它：{llm:?}");

    let (status, body) = gateway
        .request(
            reqwest::Method::DELETE,
            "/v1/config/env/KOMO_LLM_API_KEY",
            Some(serde_json::json!({ "version": after.env.version })),
        )
        .await;
    assert_eq!(status, 409, "还在用的不让删：{body}");
    let (status, body) = gateway
        .request(
            reqwest::Method::DELETE,
            "/v1/config/env/WEB_TOKEN",
            Some(serde_json::json!({ "version": after.env.version })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(!env_file().contains("WEB_TOKEN"));

    // 向量模型（§3「从 Web 加改向量模型」）：带 key 新建，key 只进 .env；视图里列出、
    // 成了 memory.embedding 的选项；删得掉
    let version = edit(&body).models.version;
    let (status, body) = gateway
        .request(
            reqwest::Method::PUT,
            "/v1/config/embedding-models/web-embed",
            Some(serde_json::json!({
                "version": version,
                "model": "qwen3-embedding:4b",
                "api_backend": "ollama_embeddings",
                "base_url": "http://127.0.0.1:11434",
                "api_key": "tok-embed-secret"
            })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("tok-embed-secret"), "响应里没有值：{body}");
    assert!(env_file().contains("WEB_EMBED_API_KEY='tok-embed-secret'"));
    let after = edit(&body);
    let added = after
        .models
        .embeddings
        .iter()
        .find(|e| e.alias == "web-embed")
        .expect("视图里有它");
    assert!(added.key_set);
    let choice = after
        .settings
        .settings
        .iter()
        .find(|s| s.key == "memory.embedding")
        .unwrap();
    assert!(choice.options.contains(&"web-embed".to_string()));
    let (status, body) = gateway
        .request(
            reqwest::Method::DELETE,
            "/v1/config/embedding-models/web-embed",
            Some(serde_json::json!({ "version": after.models.version })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        edit(&body)
            .models
            .embeddings
            .iter()
            .all(|e| e.alias != "web-embed")
    );
}
