//! 多 Agent 的会话归属、能力面与"身份"在 Durable 存储上（`docs/storage-durable-d5.md` W4b）：`../main.rs`
//! 前五个测试的 Durable 版。
//!
//! **语义改写**：Legacy 在受理时把整份身份（Profile 指令、能力面、项目规范、skills、记忆模式）冻进
//! `run.accepted`，在飞的 Run 乃至重启后恢复的 Run 都用它自己那一版。新存储没有受理快照，规矩换成：
//!
//! - **每个阶段按名字解析**：钩子 `setup` 每次请求前按**当前**配置重算 Agent、指令、能力面；改 Profile
//!   后，在飞的 Run 的**下一次请求**就是新版；
//! - **已开始的请求用 checkpoint 里冻结的模型 / effort / 预算**：请求发出（或退避重发）期间配置被改，
//!   这一次请求仍用发出时的模型；
//! - **已展示的 `project` / `skills` / `memory` 段钉住到下一次 head**（`/new`、压缩）：规范文件与 skill
//!   目录的变化不改系统提示（前缀缓存不作废），规范改动由 `submit_input` 补在下一条输入后面。
//!
//! 对应关系：
//!
//! | Legacy | Durable |
//! |---|---|
//! | `two_agents_keep_their_sessions_prompts_and_tools_apart` | 同名 |
//! | `editing_a_profile_leaves_a_run_in_flight_on_its_own_version` | `editing_a_profile_applies_from_the_next_request_of_a_run_in_flight` |
//! | `project_files_are_frozen_at_acceptance_like_the_profile` | `project_files_are_pinned_until_the_next_head_and_updates_ride_on_the_next_input` |
//! | `a_restart_resumes_a_run_on_its_own_frozen_identity` | `a_restart_resumes_an_in_flight_request_on_its_frozen_model_and_prompt` |
//! | `a_session_without_an_owner_runs_as_the_default_agent` | 同名（"没有归属的旧行"换成"归属的 Agent 已不在配置里"） |
//!
//! 停在"介入"上的 `unsure` 在这里都换成 `gate`（Run 停在一次还没返回的调用里）或"请求退避"：新存储没有
//! `waiting + intervention`。

use std::sync::Arc;

use komo_gateway::service::test_support::Gate;
use komo_gateway::service::test_support::driver_stream::{
    RecordingStream, core, open_session, submit,
};
use komo_gateway::service::test_support::harness::{
    FakeLlm, GatewayBuilder, Home, call_round, text_round,
};
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::status::RunState;

use super::{FIRST, SECOND, fed_back, request_of, tool_names, two_agents, versioned, write_call};

/// `versioned` 的 Durable 变体：能力面里的 `unsure` 换成 `gate`，模型名可指定。
fn gated(instructions: &str, model: &str) -> String {
    versioned(instructions)
        .replace("\"unsure\"", "\"gate\"")
        .replace("model = \"gpt-test\"", &format!("model = \"{model}\""))
}

/// 一个配置里两个 Agent：各自的主会话不同，系统提示里的指令不同，能用的工具不同；
/// `assistant` 调 `write` **被能力面拦住**，`coder` 调同一个调用能跑。
#[tokio::test]
async fn two_agents_keep_their_sessions_prompts_and_tools_apart() {
    let home = Home::with_config(&two_agents());
    // `coder` 的工作目录（相对路径按配置文件所在目录解析）。
    let coder_ws = home.path().join("coder-ws");
    std::fs::create_dir_all(&coder_ws).expect("工作目录");

    let llm = FakeLlm::new(vec![
        // assistant 那一段：调 `write`，能力面里没有它。
        vec![write_call(1, "notes.md"), text_round(2, "我写不了文件。")],
        // coder 那一段：同一个调用，这次能写。
        vec![write_call(1, "notes.md"), text_round(2, "写好了。")],
    ]);
    let gateway = home.start(Arc::clone(&llm)).await;
    let core = Arc::clone(gateway.state().core().expect("Durable 模式"));

    // ── 1. 主会话按 Agent 分：各一份，归属写在会话上。
    let (assistant, _) = core.main_session("assistant").await.unwrap();
    let (coder, _) = core.main_session("coder").await.unwrap();
    assert_ne!(assistant.id, coder.id, "两个 Agent 的主会话不能是同一个");
    assert_eq!(coder.agent.as_deref(), Some("coder"));
    assert_eq!(assistant.agent.as_deref(), Some("assistant"));
    // 没有归属的入口（TUI / CLI / `/v1/home-session`）走默认 Agent 那一份。
    let (code, body) = gateway.get_json("/v1/home-session").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(
        body["session"].as_str(),
        Some(assistant.id.as_str()),
        "默认 Agent 的主会话就是没有归属的入口落的那一个：{body}"
    );

    // ── 2. assistant：提示是它那一版，工具表里没有 write，于是调用被拦住。
    let run = gateway
        .submit(
            &SessionId::from_raw(&assistant.id),
            "agents-1",
            "写一份笔记",
        )
        .await
        .run;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let assistant_request = request_of(&llm, "assistant 那一版身份");
    assert_eq!(
        tool_names(&assistant_request),
        vec!["read", "rg"],
        "交给模型的 Schema 就是 Profile 挑出来的那一份"
    );
    assert!(
        !assistant_request.system_prompt.contains("coder 那一版身份"),
        "另一个 Agent 的指令不该出现在这里：{}",
        assistant_request.system_prompt
    );
    let (refusal, is_error) = fed_back(&llm, "这次运行的工具集里没有 write");
    assert!(
        is_error,
        "被能力面拦住的调用对模型是一次错误结果：{refusal}"
    );
    assert!(
        refusal.contains("可用的是"),
        "还要说清这次到底能用哪些：{refusal}"
    );
    assert!(
        !home.workspace().join("notes.md").exists() && !coder_ws.join("notes.md").exists(),
        "被拦住的 write 一个字都不许落盘"
    );

    // ── 3. coder：同一个调用，这次真的写了，而且写在**它自己的**工作目录里。
    let run = gateway
        .submit(&SessionId::from_raw(&coder.id), "agents-2", "写一份笔记")
        .await
        .run;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let coder_request = request_of(&llm, "coder 那一版身份");
    assert!(
        tool_names(&coder_request).contains(&"write".to_string()),
        "coder 的工具表里该有 write：{:?}",
        tool_names(&coder_request)
    );
    assert!(
        coder_request
            .system_prompt
            .contains(&coder_ws.display().to_string()),
        "coder 的工作目录是 Profile 里那一个：{}",
        coder_request.system_prompt
    );
    assert!(
        assistant_request
            .system_prompt
            .contains(&home.workspace().display().to_string()),
        "assistant 没有写 workspace，用的是 workspaces/：{}",
        assistant_request.system_prompt
    );
    assert!(
        coder_ws.join("notes.md").exists(),
        "coder 的 write 落在它自己的工作目录里"
    );

    gateway.stop().await;
}

/// 受理之后改 Profile（改 `instructions` 与模型）：在飞的 Run **下一次请求**就是新的那一版（每个阶段按
/// 名字解析），已经发出去的那次请求仍是发出时的；之后新受理的 Run 同样用新的。
///
/// 与 Legacy 的 `editing_a_profile_leaves_a_run_in_flight_on_its_own_version` 相反：Legacy 整份身份受理时
/// 冻结，续跑那一段仍是第一版；新存储里"冻结"只管已经开始的请求（模型 / effort / 预算在 checkpoint 里），
/// 指令与能力面每次请求前重算——操作者改了 Profile 就是希望之后的请求生效。
#[tokio::test]
async fn editing_a_profile_applies_from_the_next_request_of_a_run_in_flight() {
    let gate = Gate::new();
    let llm = FakeLlm::new(vec![
        // 第一段：先调 `gate`（Run 停在一次还没返回的调用里），再收尾。
        vec![
            call_round(1, "pc-gate", "gate", serde_json::json!({})),
            text_round(2, "跑完了。"),
        ],
        // 改完 Profile 之后新受理的那一条：只要一句话。
        vec![text_round(1, "好。")],
    ]);
    let recording =
        RecordingStream::new(Arc::clone(&llm) as Arc<dyn komo_runtime::llm::ModelStream>);
    let mut gateway = GatewayBuilder::new(&gated(FIRST, "gpt-test"))
        .fake_llm(Arc::clone(&llm))
        .stream(Arc::clone(&recording) as Arc<dyn komo_runtime::llm::ModelStream>)
        .tool(gate.tool())
        .start()
        .await;
    let session = open_session(&gateway).await;

    let first = submit(&gateway, &session, "inflight-1", "跑一下").await;
    gate.wait_entered(1).await;

    // 操作者在这时候改了 Profile 与模型——Run 还在飞（停在 gate 里）。
    gateway.write_config(&gated(SECOND, "gpt-test-2"));
    gateway.reload().await;
    gate.release();
    gateway.wait_run(&first.run).await;

    let next = submit(&gateway, &session, "inflight-2", "再说一句").await;
    gateway.wait_run(&next.run).await;

    let seen = recording.seen();
    assert_eq!(seen.len(), 3, "{seen:#?}");
    // 已经发出去的那次请求：第一版指令、发出时的模型。
    assert!(seen[0].system_prompt.contains(FIRST), "{:?}", seen[0]);
    assert!(!seen[0].system_prompt.contains(SECOND), "{:?}", seen[0]);
    assert_eq!(seen[0].model, "gpt-test");
    // 同一个 Run 的下一次请求（gate 之后）：按名字重新解析，是第二版。
    assert!(seen[1].continuing, "{:?}", seen[1]);
    assert!(seen[1].system_prompt.contains(SECOND), "{:?}", seen[1]);
    assert!(!seen[1].system_prompt.contains(FIRST), "{:?}", seen[1]);
    assert_eq!(seen[1].model, "gpt-test-2");
    // 之后新受理的 Run 同样。
    assert!(seen[2].system_prompt.contains(SECOND), "{:?}", seen[2]);
    assert_eq!(seen[2].model, "gpt-test-2");

    gateway.stop().await;
}

/// 工作目录里的 `AGENTS.md` 进主 Agent 的提示；Run 在飞时它被改了，续跑那一次请求与同一会话的下一条 Run
/// 的系统提示都**沿用已展示的那一版**（`project` 段钉住到下一次 head，前缀缓存不作废），新的那一版补在
/// 下一条输入的后面——同一版只补一次，又改了才再补；`/new` 之后系统提示才换成新版，也不再补。
#[tokio::test]
async fn project_files_are_pinned_until_the_next_head_and_updates_ride_on_the_next_input() {
    const OLD: &str = "仓库规范第一版：先读再改。";
    const NEW: &str = "仓库规范第二版：改完跑测试。";
    const NEWER: &str = "仓库规范第三版：提交前跑 fmt。";
    let gate = Gate::new();
    let llm = FakeLlm::new(vec![
        vec![
            call_round(1, "pc-gate", "gate", serde_json::json!({})),
            text_round(2, "跑完了。"),
        ],
        vec![text_round(1, "好。")],
        vec![text_round(1, "好。")],
        vec![text_round(1, "好。")],
        vec![text_round(1, "好。")],
    ]);
    let recording =
        RecordingStream::new(Arc::clone(&llm) as Arc<dyn komo_runtime::llm::ModelStream>);
    let home = Home::with_config(&gated(FIRST, "gpt-test"));
    let mut gateway = GatewayBuilder::new(&gated(FIRST, "gpt-test"))
        .fake_llm(Arc::clone(&llm))
        .stream(Arc::clone(&recording) as Arc<dyn komo_runtime::llm::ModelStream>)
        .tool(gate.tool())
        .at(home.path())
        .start()
        .await;
    // `workspaces/` 由 Gateway 启动时建好；规范写在它里面（会话没有 workdir、Profile 没写 workspace 时
    // 就是这个目录）。
    let agents_md = home.workspace().join("AGENTS.md");
    std::fs::write(&agents_md, OLD).expect("写 AGENTS.md");
    let session = open_session(&gateway).await;

    let first = submit(&gateway, &session, "project-1", "跑一下").await;
    gate.wait_entered(1).await;
    std::fs::write(&agents_md, NEW).expect("改 AGENTS.md");
    gate.release();
    gateway.wait_run(&first.run).await;

    let next = submit(&gateway, &session, "project-2", "再说一句").await;
    gateway.wait_run(&next.run).await;

    let seen = recording.seen();
    assert_eq!(seen.len(), 3, "{seen:#?}");
    assert!(
        seen.iter()
            .all(|request| request.system_prompt == seen[0].system_prompt
                && request.system_prompt.contains(OLD)
                && request
                    .system_prompt
                    .contains("<project_instructions path=")),
        "三次请求的系统提示逐字相同，都是已展示的那一版：{seen:#?}"
    );
    assert!(
        seen.iter()
            .all(|request| !request.system_prompt.contains(NEW))
    );

    // 新的那一版补在下一条输入后面；没改过时的第一条输入不补。
    let inputs = user_inputs(&gateway, &session).await;
    assert_eq!(inputs.len(), 2, "{inputs:?}");
    assert!(
        !inputs[0].contains("项目规范在本会话中改过"),
        "没改过的输入不补：{}",
        inputs[0]
    );
    assert!(
        inputs[1].starts_with("再说一句") && inputs[1].contains("项目规范在本会话中改过"),
        "新的那一版补在新输入后面：{}",
        inputs[1]
    );
    assert!(
        inputs[1].contains(NEW) && !inputs[1].contains(OLD),
        "{}",
        inputs[1]
    );
    // 补的那段只给模型：entry 记下原话多长，界面与记忆抽取按它切回"再说一句"。
    let marks: Vec<Option<u64>> = user_entries(&gateway, &session)
        .await
        .into_iter()
        .map(|(_, at)| at)
        .collect();
    assert_eq!(marks, [None, Some("再说一句".len() as u64)]);

    // 同一版已经补过：下一条不再补。
    let third = submit(&gateway, &session, "project-2b", "第三句").await;
    gateway.wait_run(&third.run).await;
    // 又改了：再补一次，补的是最新那一版。
    std::fs::write(&agents_md, NEWER).expect("再改 AGENTS.md");
    let fourth = submit(&gateway, &session, "project-2c", "第四句").await;
    gateway.wait_run(&fourth.run).await;
    let inputs = user_inputs(&gateway, &session).await;
    assert_eq!(inputs.len(), 4, "{inputs:?}");
    assert_eq!(inputs[2], "第三句", "同一版不补第二次");
    assert!(
        inputs[3].starts_with("第四句") && inputs[3].contains(NEWER) && !inputs[3].contains(NEW),
        "又改了才再补，补最新的：{}",
        inputs[3]
    );

    // `/new`：下一次 head 才换系统提示，也不再补更新块。
    core(&gateway).boundary(&session, None).await.expect("/new");
    let after = submit(&gateway, &session, "project-3", "重新开始").await;
    gateway.wait_run(&after.run).await;
    let seen = recording.seen();
    let latest = seen.last().expect("有请求");
    assert!(
        latest.system_prompt.contains(NEWER) && !latest.system_prompt.contains(OLD),
        "head 之后换成新版：{}",
        latest.system_prompt
    );
    let inputs = user_inputs(&gateway, &session).await;
    assert!(
        !inputs
            .last()
            .expect("有输入")
            .contains("项目规范在本会话中改过"),
        "新 head 里系统提示就是新版，不补：{inputs:?}"
    );

    gateway.stop().await;
}

/// 一个 Session 里全部用户输入的正文（按受理顺序）。
async fn user_inputs(
    gateway: &komo_gateway::service::test_support::harness::TestGateway,
    session: &str,
) -> Vec<String> {
    user_entries(gateway, session)
        .await
        .into_iter()
        .map(|(text, _)| text)
        .collect()
}

/// 每条用户输入的正文与 `data.projectUpdateAt`（补了项目规范时，用户原话的字节长度）。
async fn user_entries(
    gateway: &komo_gateway::service::test_support::harness::TestGateway,
    session: &str,
) -> Vec<(String, Option<u64>)> {
    use komo_kernel::durable::legacy_events::PROJECT_UPDATE_AT;
    use komo_kernel::durable::storage::{ConversationQuery, EntryQuery};
    use komo_kernel::durable::{Content, ContentBlock, Message};
    let core = core(gateway);
    let mut conversations = core
        .storage
        .scan_conversations(
            &ConversationQuery {
                session: Some(session.to_owned()),
                ..ConversationQuery::default()
            },
            1_000,
            None,
        )
        .await
        .expect("读得出")
        .items;
    conversations.sort_by_key(|conversation| conversation.id);
    let mut texts = Vec::new();
    for conversation in conversations {
        let mut entries = core
            .storage
            .scan_entries(&EntryQuery::new(conversation.id), 10_000, None)
            .await
            .expect("读得出")
            .items;
        entries.sort_by_key(|stored| stored.id);
        for stored in entries {
            for message in stored.model.iter().flatten() {
                if let Message::User(user) = message {
                    let at = stored
                        .data
                        .as_ref()
                        .and_then(|data| data.get(PROJECT_UPDATE_AT))
                        .and_then(|at| at.as_u64());
                    let text = match &user.content {
                        Content::Text(text) => text.clone(),
                        Content::Blocks(blocks) => blocks
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text { text, .. } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    };
                    texts.push((text, at));
                }
            }
        }
    }
    texts
}

/// 重启时正有一次请求在飞：恢复后**重发同一份请求**——用发出时冻结的模型与消息（checkpoint 里的
/// `cutoff` 之前的 entry，系统段也在其中），而不是磁盘上现在这一版；同一个 Run 的下一次请求再按名字
/// 重新解析，是新的一版。
///
/// 对应 Legacy 的 `a_restart_resumes_a_run_on_its_own_frozen_identity`：那里停在"结果不明"的介入上、重启后
/// 用 `run.accepted` 里的整份身份。新存储停在介入上的状态不存在，能在重启后"接着跑"的是在飞的请求；
/// "冻结"缩到请求这一粒度（退避重发走 `prepare`，会重新解析，不在此列）。
#[tokio::test]
async fn a_restart_resumes_an_in_flight_request_on_its_frozen_model_and_prompt() {
    let llm = FakeLlm::new(vec![vec![text_round(1, "不该走到这里")]]);
    let held = RecordingStream::new(Arc::clone(&llm) as Arc<dyn komo_runtime::llm::ModelStream>);
    held.hold_next();
    let mut first = GatewayBuilder::new(&gated(FIRST, "gpt-test"))
        .fake_llm(Arc::clone(&llm))
        .stream(Arc::clone(&held) as Arc<dyn komo_runtime::llm::ModelStream>)
        .start()
        .await;
    let session = open_session(&first).await;
    let run = submit(&first, &session, "restart-1", "跑一下").await.run;
    held.wait_seen(1).await;
    // 请求已经发出（记下来了）、还没回来：停机。
    first.stop().await;

    // 停机期间 Profile 与模型都被改过（磁盘上那一份已经不一样了）。
    let resumed_llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-echo",
            "bash",
            serde_json::json!({ "command": "echo hi" }),
        ),
        text_round(2, "跑完了。"),
    ]]);
    let recording =
        RecordingStream::new(Arc::clone(&resumed_llm) as Arc<dyn komo_runtime::llm::ModelStream>);
    let mut second = GatewayBuilder::new(&gated(SECOND, "gpt-test-2"))
        .fake_llm(Arc::clone(&resumed_llm))
        .stream(Arc::clone(&recording) as Arc<dyn komo_runtime::llm::ModelStream>)
        .at(&first.home)
        .start()
        .await;
    second.wait_run(&run).await;

    let seen = recording.seen();
    assert_eq!(seen.len(), 2, "{seen:#?}");
    assert_eq!(
        seen[0].model, "gpt-test",
        "在飞的请求用 checkpoint 里冻结的模型，不是磁盘上现在这一个"
    );
    assert!(
        seen[0].system_prompt.contains(FIRST) && !seen[0].system_prompt.contains(SECOND),
        "重发的是同一份消息，系统段也是发出时那一版：{}",
        seen[0].system_prompt
    );
    assert!(seen[1].continuing, "{:?}", seen[1]);
    assert_eq!(seen[1].model, "gpt-test-2", "下一次请求按当前配置解析");
    assert!(
        seen[1].system_prompt.contains(SECOND) && !seen[1].system_prompt.contains(FIRST),
        "{}",
        seen[1].system_prompt
    );

    second.stop().await;
}

/// 没有 Agent 这一维的会话走 `default_agent`：`POST /v1/sessions` 建的那种**建的时候就记在默认 Agent
/// 名下**（§4.2）；归属的 Agent 后来不在配置里了（旧行 / 配置删改）也按当前默认 Agent 用。
///
/// 改了做法：Legacy 的"升级前还没有归属的行"在新存储里不存在（§8 不迁移），换成等价的另一半——会话
/// 绑的 Agent 名在配置里找不到，钩子退回默认 Agent（`hooks/setup.rs`），而且**不改写那一行**。
#[tokio::test]
async fn a_session_without_an_owner_runs_as_the_default_agent() {
    let home = Home::with_config(&two_agents());
    let llm = FakeLlm::new(vec![
        vec![text_round(1, "好。")],
        vec![text_round(1, "好。")],
    ]);
    let gateway = home.start(Arc::clone(&llm)).await;
    let core = Arc::clone(gateway.state().core().expect("Durable 模式"));

    // ① `POST /v1/sessions`：归属在建的时候就写下来了。
    let created = gateway.open_session().await;
    assert_eq!(
        core.session(created.as_str())
            .await
            .unwrap()
            .agent
            .as_deref(),
        Some("assistant"),
        "建会话时就记在默认 Agent 名下"
    );

    // ② 绑着一个配置里没有的 Agent：按当前默认 Agent 用它，而且不改写那一行。
    let orphan = core
        .create_session(komo_gateway::service::durable::sessions::NewSession::new(
            "ghost",
        ))
        .await
        .expect("建一个归属不在配置里的会话");
    for (session, key) in [
        (created.as_str(), "default-1"),
        (orphan.id.as_str(), "default-2"),
    ] {
        let run = gateway
            .submit(&SessionId::from_raw(session), key, "在吗")
            .await
            .run;
        gateway.wait_terminal(&run).await;
    }
    assert_eq!(
        core.session(&orphan.id).await.unwrap().agent.as_deref(),
        Some("ghost"),
        "归属只写一次，不被默认 Agent 覆盖"
    );
    let requests = llm.requests.lock().expect("脚本模型").clone();
    assert_eq!(requests.len(), 2, "两条各一段");
    for request in requests {
        assert!(
            request.system_prompt.contains("assistant 那一版身份"),
            "两段都是默认 Agent 的身份：{}",
            request.system_prompt
        );
    }

    gateway.stop().await;
}
