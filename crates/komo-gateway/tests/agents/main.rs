//! 多 Agent：**会话归属、能力面、以及受理时冻结的那一份身份**（`docs/bot.md` §四）。
//!
//! 这一组守着三件事，每一件都是一个"两份事实"的缺口：
//!
//! - **主会话按 Agent 各一份**（`main_session(agent_id)`）：没有归属的入口（TUI / CLI /
//!   `/v1/home-session`）走 `default_agent`，而不是所有人共用一个全局 home session；
//! - **能力面来自 Profile**：系统提示里的指令、交给模型的工具 Schema、以及执行器认不认
//!   这个名字，三者同源——`assistant` 调 `write` 得到的是"这次运行的工具集里没有它"，
//!   不是"先试一下再说"；
//! - **受理时冻结**（§4.3）：Profile 在 Run 在飞的时候被改过（甚至整台 Gateway 重启过），
//!   那一条 Run 仍然用它自己那一版身份跑完，下一条 Run 才用新的。
//!
//! 共用件在 `komo_gateway::service::test_support::harness`（真数据目录、真 `service::start`、
//! 脚本化模型）。

use std::sync::Arc;

use komo_gateway::service::test_support::harness::{
    DEFAULT_ENV, FakeLlm, Home, call_round, text_round, write_home_with,
};
use komo_kernel::traits::LlmClient;
use komo_kernel::types::status::RunState;
use komo_kernel::types::turn::{LlmError, Round, RoundInput, TurnRequest};

// ---------------------------------------------------------------- 配置

/// 两个 Agent：`assistant` 只能读，`coder` 能写，而且各有各的工作目录。
///
/// `default_agent = "assistant"`：没有归属的入口走它（§四）。
fn two_agents() -> String {
    r#"
default_agent = "assistant"

[agents.assistant]
instructions = "你是助手（assistant 那一版身份）：先读，再答。"
tools = ["read", "rg"]

[agents.coder]
instructions = "你是编码助手（coder 那一版身份）：可以直接写文件。"
tools = ["read", "write", "edit", "rg"]
workspace = "coder-ws"

[model.main]
type = "completion"
api_backend = "responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[models]
default = "main"

[memory]
enabled = false
"#
    .to_string()
}

/// 只差 `instructions` 的两版配置：`assistant` 有 shell（默认规则下 shell 要问，于是
/// Run 会停在审批上——那正是"在飞"的样子）。
fn versioned(instructions: &str) -> String {
    format!(
        r#"
default_agent = "assistant"

[agents.assistant]
instructions = "{instructions}"
tools = ["read", "rg", "shell"]

[model.main]
type = "completion"
api_backend = "responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[models]
default = "main"

[memory]
enabled = false
"#
    )
}

const FIRST: &str = "第一版身份：你是谁只由这一句说清。";
const SECOND: &str = "第二版身份：这一句在受理之后才写进配置。";

/// 停机之后改 `config.toml`（重启用的那一份）。
fn rewrite_config(home: &Home, config: &str) {
    write_home_with(home.path(), config, DEFAULT_ENV, None);
    // mtime 的粒度可能是秒：把时间推一下，`changed_since` 才看得出来（与
    // `TestGateway::write_config` 同一个做法）。
    let path = home.path().join("config.toml");
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
    let _ = std::fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|file| file.set_times(std::fs::FileTimes::new().set_modified(later)));
}

// ---------------------------------------------------------------- 断言助手

/// 提示里带着某个标记的那一次模型请求（一个执行段一次请求，标记够用）。
fn request_of(llm: &FakeLlm, marker: &str) -> TurnRequest {
    llm.requests
        .lock()
        .expect("脚本模型")
        .iter()
        .find(|request| request.system_prompt.contains(marker))
        .cloned()
        .unwrap_or_else(|| panic!("没有哪一次请求的提示里带着 {marker}"))
}

fn tool_names(request: &TurnRequest) -> Vec<String> {
    request.tools.iter().map(|tool| tool.name.clone()).collect()
}

/// 模型被喂回来的一条工具结果（工具结果从轮输入回去）：正文里带着这段文字的那一条。
///
/// 被能力面拦住的调用**没有计划**，所以交回模型的是执行器那句话本身，不是投影出来的
/// `[工具 · 状态 · 耗时]` 抬头。
fn fed_back(llm: &FakeLlm, needle: &str) -> (String, bool) {
    llm.inputs
        .lock()
        .expect("轮输入")
        .iter()
        .find_map(|input| match input {
            RoundInput::ToolResults { results } => results
                .iter()
                .find(|result| result.content.contains(needle))
                .map(|result| (result.content.clone(), result.is_error)),
            RoundInput::First => None,
        })
        .unwrap_or_else(|| panic!("模型没拿到带 `{needle}` 的结果"))
}

/// 一次 `write` 调用（路径相对这一段的工作目录）。
fn write_call(round: u32, path: &str) -> Result<Round, LlmError> {
    call_round(
        round,
        "pc-write",
        "write",
        serde_json::json!({ "path": path, "content": "# 笔记\n" }),
    )
}

// ---------------------------------------------------------------- 各自的会话、指令与工具

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
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    // ── 1. 主会话按 Agent 分：各一份，归属写在会话上。
    let assistant = gateway.state().main_session("assistant").await.unwrap();
    let coder = gateway.state().main_session("coder").await.unwrap();
    assert_ne!(assistant, coder, "两个 Agent 的主会话不能是同一个");
    assert_eq!(
        gateway.state().agent_of_session(&coder).await.unwrap(),
        "coder"
    );
    assert_eq!(
        gateway.state().agent_of_session(&assistant).await.unwrap(),
        "assistant"
    );
    // 没有归属的入口（TUI / CLI / `/v1/home-session`）走默认 Agent 那一份。
    assert_eq!(
        gateway.state().default_main_session().await.unwrap(),
        assistant,
        "默认 Agent 的主会话就是没有归属的入口落的那一个"
    );

    // ── 2. assistant：提示是它那一版，工具表里没有 write，于是调用被拦住。
    let run = gateway
        .submit(&assistant, "agents-1", "写一份笔记")
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
    let run = gateway.submit(&coder, "agents-2", "写一份笔记").await.run;
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

// ---------------------------------------------------------------- 在飞的 Run 不受 Profile 改动影响

/// 受理之后改 Profile（改 `instructions`）→ **在飞的那条 Run 不受影响**，下一条 Run 用新的。
#[tokio::test]
async fn editing_a_profile_leaves_a_run_in_flight_on_its_own_version() {
    let home = Home::with_config(&versioned(FIRST));
    let llm = FakeLlm::new(vec![
        // 第一段：要一次 shell（默认规则下要问），于是 Run 停在审批上。
        vec![call_round(
            1,
            "pc-sh",
            "shell",
            serde_json::json!({ "command": "echo hi" }),
        )],
        // 批准之后的续跑段：只说一句话收尾。
        vec![text_round(2, "跑完了。")],
        // 改完 Profile 之后新受理的那一条：只要一句话。
        vec![text_round(1, "好。")],
    ]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.state().default_main_session().await.unwrap();

    let run = gateway.submit(&session, "inflight-1", "跑一下").await.run;
    let approval = gateway.wait_approval().await;

    // 操作者在这时候改了 Profile——Run 还在飞（停在审批上）。
    rewrite_config(&home, &versioned(SECOND));
    komo_gateway::reload::reload(gateway.state())
        .await
        .expect("新配置装得上");

    // 批准：续跑那一段用的还是**受理那一刻**冻结下来的那一版身份。
    gateway.decide(&approval.approval, true).await;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let resumed = request_of(&llm, "第一版身份");
    assert!(
        !resumed.system_prompt.contains(SECOND),
        "在飞的 Run 不该被换一副面孔：{}",
        resumed.system_prompt
    );

    // 下一条 Run：受理时读的是**新**那一版。
    let next = gateway.submit(&session, "inflight-2", "再说一句").await.run;
    gateway.wait_terminal(&next).await;
    let fresh = request_of(&llm, SECOND);
    assert!(
        !fresh.system_prompt.contains(FIRST),
        "新 Run 用新那一版：{}",
        fresh.system_prompt
    );

    gateway.stop().await;
}

// ---------------------------------------------------------------- 重启之后仍然用它自己那一版

/// 重启 Gateway 恢复同一条 Run：身份来自**它自己的 `run.accepted`**，不是重启后磁盘上
/// 那份已经改过的 Profile。
#[tokio::test]
async fn a_restart_resumes_a_run_on_its_own_frozen_identity() {
    let home = Home::with_config(&versioned(FIRST));
    let llm = FakeLlm::new(vec![vec![call_round(
        1,
        "pc-sh",
        "shell",
        serde_json::json!({ "command": "echo hi" }),
    )]]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.state().default_main_session().await.unwrap();
    let run = gateway.submit(&session, "restart-1", "跑一下").await.run;
    let approval = gateway.wait_approval().await;
    gateway.stop().await;

    // 停机期间 Profile 被改过（磁盘上那一份已经不一样了）。
    rewrite_config(&home, &versioned(SECOND));

    let resumed_llm = FakeLlm::new(vec![vec![text_round(2, "跑完了。")]]);
    let gateway = home
        .start(Arc::clone(&resumed_llm) as Arc<dyn LlmClient>)
        .await;
    gateway.decide(&approval.approval, true).await;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let resumed = request_of(&resumed_llm, "第一版身份");
    assert!(
        !resumed.system_prompt.contains(SECOND),
        "恢复用的是它自己那一版身份，不是磁盘上现在这一版：{}",
        resumed.system_prompt
    );
    assert_eq!(
        tool_names(&resumed),
        vec!["read", "rg", "shell"],
        "能力面也是冻结的那一份"
    );

    gateway.stop().await;
}

/// 没有 Agent 这一维的会话走 `default_agent`：`POST /v1/sessions` 建的那种**建的时候就
/// 记在默认 Agent 名下**（§4.2），而升级前那些还没有归属的行按当前默认 Agent 用。
#[tokio::test]
async fn a_session_without_an_owner_runs_as_the_default_agent() {
    let home = Home::with_config(&two_agents());
    let llm = FakeLlm::new(vec![
        vec![text_round(1, "好。")],
        vec![text_round(1, "好。")],
    ]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    // ① `POST /v1/sessions`：归属在建的时候就写下来了。
    let created = gateway.open_session().await;
    assert_eq!(
        gateway.state().agent_of_session(&created).await.unwrap(),
        "assistant",
        "建会话时就记在默认 Agent 名下"
    );

    // ② 一行**还没有归属**的会话（升级前建的行就是这样）：按当前默认 Agent 用它，
    //    而且**不改写那一行**——归属只写一次。
    let legacy = komo_kernel::types::ids::SessionId::new_at(gateway.state().clock.now());
    gateway
        .state()
        .ledgers
        .open(&legacy, "agent")
        .await
        .expect("开一个还没有归属的会话");
    assert_eq!(
        gateway.state().agent_of_session(&legacy).await.unwrap(),
        "assistant",
        "没有归属的会话走 default_agent"
    );

    for (session, key) in [(&created, "default-1"), (&legacy, "default-2")] {
        let run = gateway.submit(session, key, "在吗").await.run;
        gateway.wait_terminal(&run).await;
    }
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

// ---------------------------------------------------------------- [home] mode = "dispatch"（docs/home-dispatcher.md §3、§9 Phase 1）

/// `default_agent = "assistant"`、`[home] dispatcher = "dispatcher"`：home session 的
/// Run 该用分发器身份，别的会话不受影响。
fn home_dispatch_config() -> String {
    r#"
default_agent = "assistant"

[agents.assistant]
instructions = "你是助手（assistant 那一版身份）：先读，再答。"
tools = ["read", "rg"]

[agents.dispatcher]
instructions = "你是分发器（dispatcher 那一版身份）：能答就答，需要动手就派任务。"
tools = ["read"]

[home]
mode = "dispatch"
dispatcher = "dispatcher"

[model.main]
type = "completion"
api_backend = "responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[models]
default = "main"

[memory]
enabled = false
"#
    .to_string()
}

/// 同上，但 `dispatcher` 多一个 `shell`（用来造一次要审批的调用，测"在飞的 Run 不受
/// 热重载影响"）。
fn home_dispatch_config_with_shell() -> String {
    home_dispatch_config().replace(
        "[agents.dispatcher]\ninstructions = \"你是分发器（dispatcher 那一版身份）：能答就答，需要动手就派任务。\"\ntools = [\"read\"]",
        "[agents.dispatcher]\ninstructions = \"你是分发器（dispatcher 那一版身份）：能答就答，需要动手就派任务。\"\ntools = [\"read\", \"shell\"]",
    )
}

/// 同一份配置，`[home] mode = "session"`（今天的行为）——用来测热重载切回去。
fn home_session_config() -> String {
    home_dispatch_config().replace("mode = \"dispatch\"", "mode = \"session\"")
}

/// `mode = "dispatch"`：home session 的 Run 用 `dispatcher` 那一版身份（指令、工具表）；
/// 同一台 Gateway 里一个普通会话仍然用它自己的 Agent——`[home]` 只管 home session。
#[tokio::test]
async fn a_home_session_in_dispatch_mode_uses_the_dispatcher_profile() {
    let home = Home::with_config(&home_dispatch_config());
    let llm = FakeLlm::new(vec![
        // home session：分发器那一段。
        vec![text_round(1, "好，交给我。")],
        // 普通会话：assistant 自己的那一段。
        vec![text_round(1, "好。")],
    ]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    // home session：`default_main_session` 就是它——`kind = main` 且 `origin = home`。
    let home_session = gateway.state().default_main_session().await.unwrap();
    let run = gateway
        .submit(&home_session, "home-1", "查一下空调状态")
        .await
        .run;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let dispatcher_request = request_of(&llm, "dispatcher 那一版身份");
    assert_eq!(
        tool_names(&dispatcher_request),
        vec!["read"],
        "home session 冻结的是 dispatcher 的能力面：{:?}",
        tool_names(&dispatcher_request)
    );
    assert!(
        !dispatcher_request
            .system_prompt
            .contains("assistant 那一版身份"),
        "home session 不该带着 assistant 的指令：{}",
        dispatcher_request.system_prompt
    );

    // 会话的 `agent_id` 没有变：它仍然归 `assistant`（§3：「home 会话的 agent_id 不变」）。
    assert_eq!(
        gateway
            .state()
            .agent_of_session(&home_session)
            .await
            .unwrap(),
        "assistant"
    );

    // 一个普通会话（`POST /v1/sessions` 建的）不受 `[home]` 影响，仍然用它自己的 Agent。
    let normal_session = gateway.open_session().await;
    let run = gateway
        .submit(&normal_session, "normal-1", "在吗")
        .await
        .run;
    gateway.wait_terminal(&run).await;
    let assistant_request = request_of(&llm, "assistant 那一版身份");
    assert_eq!(
        tool_names(&assistant_request),
        vec!["read", "rg"],
        "普通会话用的是 assistant 自己的能力面"
    );

    gateway.stop().await;
}

/// 热重载把 `mode` 从 `dispatch` 切回 `session`：下一条 home Run 用回默认 Agent。
#[tokio::test]
async fn switching_home_mode_back_to_session_on_reload_uses_the_default_agent_next() {
    let home = Home::with_config(&home_dispatch_config());
    let llm = FakeLlm::new(vec![
        // 第一段：mode = dispatch，用 dispatcher 身份。
        vec![text_round(1, "交给我。")],
        // 第二段：切回 session 之后，用 assistant 身份。
        vec![text_round(1, "好。")],
    ]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let home_session = gateway.state().default_main_session().await.unwrap();

    let run = gateway
        .submit(&home_session, "switch-1", "第一句")
        .await
        .run;
    gateway.wait_terminal(&run).await;
    let first = request_of(&llm, "dispatcher 那一版身份");
    assert_eq!(tool_names(&first), vec!["read"]);

    // 操作者把 `[home] mode` 改回 `session` 并热重载。
    rewrite_config(&home, &home_session_config());
    komo_gateway::reload::reload(gateway.state())
        .await
        .expect("新配置装得上");

    let run = gateway
        .submit(&home_session, "switch-2", "第二句")
        .await
        .run;
    gateway.wait_terminal(&run).await;
    let second = request_of(&llm, "assistant 那一版身份");
    assert_eq!(
        tool_names(&second),
        vec!["read", "rg"],
        "切回 session 之后，下一条 home Run 用默认 Agent 的能力面"
    );
    assert!(
        !second.system_prompt.contains("dispatcher 那一版身份"),
        "{}",
        second.system_prompt
    );

    gateway.stop().await;
}

/// home session 受理时用 `dispatch` 冻结身份，之后热重载把 `mode` 改回 `session`——
/// **在飞的那条 Run** 仍然用它受理那一刻冻结的 dispatcher 身份跑完（§4.3、§9 Phase 1
/// 的"run 在飞时改配置不换脸"，这里换的是 `[home] mode` 而不是 Profile 本身）。
#[tokio::test]
async fn a_home_run_in_flight_keeps_the_dispatcher_snapshot_across_a_mode_switch() {
    let home = Home::with_config(&home_dispatch_config_with_shell());
    let llm = FakeLlm::new(vec![
        // 第一段：调一次 shell（默认规则下要问），Run 停在审批上。
        vec![call_round(
            1,
            "pc-sh",
            "shell",
            serde_json::json!({ "command": "echo hi" }),
        )],
        // 批准之后的续跑段。
        vec![text_round(2, "跑完了。")],
        // 切回 session 之后新受理的那一条。
        vec![text_round(1, "好。")],
    ]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let home_session = gateway.state().default_main_session().await.unwrap();

    let run = gateway
        .submit(&home_session, "inflight-1", "跑一下")
        .await
        .run;
    let approval = gateway.wait_approval().await;

    // Run 还在飞（停在审批上）的时候，操作者把 `[home] mode` 改回 `session`。
    rewrite_config(&home, &home_session_config());
    komo_gateway::reload::reload(gateway.state())
        .await
        .expect("新配置装得上");

    gateway.decide(&approval.approval, true).await;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let resumed = request_of(&llm, "dispatcher 那一版身份");
    assert_eq!(
        tool_names(&resumed),
        vec!["read", "shell"],
        "在飞的 Run 用的是受理那一刻冻结的 dispatcher 能力面：{:?}",
        tool_names(&resumed)
    );

    // 下一条 Run：受理时读的是新快照（`mode = session`），用默认 Agent。
    let next = gateway
        .submit(&home_session, "inflight-2", "再说一句")
        .await
        .run;
    gateway.wait_terminal(&next).await;
    let fresh = request_of(&llm, "assistant 那一版身份");
    assert!(
        !fresh.system_prompt.contains("dispatcher 那一版身份"),
        "{}",
        fresh.system_prompt
    );

    gateway.stop().await;
}
