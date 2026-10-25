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

use std::sync::{Arc, Mutex};

use komo_gateway::service::test_support::Unsure;
use komo_gateway::service::test_support::harness::{
    DEFAULT_ENV, FakeLlm, Gw, Home, MemSender, call_round, eventually, inbound, text_round,
    write_home,
};
use komo_kernel::protocol::InboundAck;
use komo_kernel::traits::{Inbound, LlmClient, TurnDriver};
use komo_kernel::types::chat::{ChannelPlatform, Outbound};
use komo_kernel::types::ids::{RunId, SessionId};
use komo_kernel::types::model::TokenUsage;
use komo_kernel::types::status::{RunState, WaitReason};
use komo_kernel::types::turn::{LlmError, Round, RoundInput, TurnRequest};

mod durable;

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

/// 只差 `instructions` 的两版配置：`assistant` 有测试工具 `unsure`（一调就结果不明，于是
/// Run 会停在一条介入上——那正是"在飞"的样子）。
fn versioned(instructions: &str) -> String {
    format!(
        r#"
default_agent = "assistant"

[agents.assistant]
instructions = "{instructions}"
tools = ["read", "rg", "bash", "unsure"]

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
    write_home(home.path(), config, DEFAULT_ENV);
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
            RoundInput::ToolResults { results, .. } => results
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
    home.install_tools(vec![Unsure::new().tool()]);
    let llm = FakeLlm::new(vec![
        // 第一段：调一次 `unsure`（结果不明），于是 Run 停在一条介入上。
        vec![call_round(1, "pc-sh", "unsure", serde_json::json!({}))],
        // 核对之后的续跑段：只说一句话收尾。
        vec![text_round(2, "跑完了。")],
        // 改完 Profile 之后新受理的那一条：只要一句话。
        vec![text_round(1, "好。")],
    ]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.state().default_main_session().await.unwrap();

    let run = gateway.submit(&session, "inflight-1", "跑一下").await.run;
    let pending = gateway.wait_intervention(&run).await;

    // 操作者在这时候改了 Profile——Run 还在飞（停在介入上）。
    rewrite_config(&home, &versioned(SECOND));
    komo_gateway::reload::reload(gateway.state())
        .await
        .expect("新配置装得上");

    // 答复：续跑那一段用的还是**受理那一刻**冻结下来的那一版身份。
    gateway.answer(&pending.handle, "satisfied").await;
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

// ---------------------------------------------------------------- 项目规范同样受理时冻结

/// 工作目录里的 `AGENTS.md` 进主 Agent 的提示；Run 在飞时它被改了，续跑那一段仍是
/// **受理时**那一版。同一会话的下一条 Run 系统提示仍沿用那一版（前缀缓存不作废），新的
/// 那一版补在它的任务后面。
#[tokio::test]
async fn project_files_are_frozen_at_acceptance_like_the_profile() {
    const OLD: &str = "仓库规范第一版：先读再改。";
    const NEW: &str = "仓库规范第二版：改完跑测试。";
    let home = Home::with_config(&versioned(FIRST));
    home.install_tools(vec![Unsure::new().tool()]);
    let llm = FakeLlm::new(vec![
        vec![call_round(1, "pc-sh", "unsure", serde_json::json!({}))],
        vec![text_round(2, "跑完了。")],
        vec![text_round(1, "好。")],
    ]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    // `workspaces/` 由 Gateway 启动时建好；规范写在它里面（会话没有 workdir、Profile
    // 没写 workspace 时就是这个目录）。
    let agents_md = home.workspace().join("AGENTS.md");
    std::fs::write(&agents_md, OLD).expect("写 AGENTS.md");
    let session = gateway.state().default_main_session().await.unwrap();

    let run = gateway.submit(&session, "project-1", "跑一下").await.run;
    let pending = gateway.wait_intervention(&run).await;
    std::fs::write(&agents_md, NEW).expect("改 AGENTS.md");
    gateway.answer(&pending.handle, "satisfied").await;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let next = gateway.submit(&session, "project-2", "再说一句").await.run;
    gateway.wait_terminal(&next).await;

    let requests = llm.requests.lock().expect("脚本模型").clone();
    let prompts: Vec<&String> = requests
        .iter()
        .map(|request| &request.system_prompt)
        .collect();
    assert_eq!(prompts.len(), 3, "{prompts:#?}");
    assert!(
        prompts.iter().all(|prompt| *prompt == prompts[0]
            && prompt.contains(OLD)
            && prompt.contains("<project_instructions path=")),
        "三次请求的系统提示逐字相同，都是受理时那一版：{prompts:#?}"
    );
    assert!(prompts.iter().all(|prompt| !prompt.contains(NEW)));

    let task = |request: &komo_kernel::types::turn::TurnRequest| {
        request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == komo_kernel::types::turn::Role::User)
            .and_then(|message| message.text.clone())
            .unwrap_or_default()
    };
    let latest = task(&requests[2]);
    assert!(
        latest.starts_with("再说一句") && latest.contains("项目规范在本会话中改过"),
        "新那一版补在新 Run 的任务后面：{latest}"
    );
    assert!(latest.contains(NEW) && !latest.contains(OLD), "{latest}");
    assert!(
        !task(&requests[0]).contains("项目规范在本会话中改过"),
        "没改过的 Run 不补"
    );

    gateway.stop().await;
}

// ---------------------------------------------------------------- 重启之后仍然用它自己那一版

/// 重启 Gateway 恢复同一条 Run：身份来自**它自己的 `run.accepted`**，不是重启后磁盘上
/// 那份已经改过的 Profile。
#[tokio::test]
async fn a_restart_resumes_a_run_on_its_own_frozen_identity() {
    let home = Home::with_config(&versioned(FIRST));
    home.install_tools(vec![Unsure::new().tool()]);
    let llm = FakeLlm::new(vec![vec![call_round(
        1,
        "pc-sh",
        "unsure",
        serde_json::json!({}),
    )]]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.state().default_main_session().await.unwrap();
    let run = gateway.submit(&session, "restart-1", "跑一下").await.run;
    let pending = gateway.wait_intervention(&run).await;
    gateway.stop().await;

    // 停机期间 Profile 被改过（磁盘上那一份已经不一样了）。
    rewrite_config(&home, &versioned(SECOND));

    let resumed_llm = FakeLlm::new(vec![vec![text_round(2, "跑完了。")]]);
    let gateway = home
        .start(Arc::clone(&resumed_llm) as Arc<dyn LlmClient>)
        .await;
    gateway.answer(&pending.handle, "satisfied").await;
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
        vec!["read", "rg", "bash", "unsure"],
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

// ---------------------------------------------------------------- 后台任务（`docs/background-tasks.md`）

/// home 用的就是它自己的 Agent：能派后台任务（`task`），也能自己动手。两个模型：
/// `main` 是默认，`cheap` 给任务用。渠道是 Telegram（操作者 111，home chat 也是 111），
/// DM 走真 Dispatcher，不绕过它。
fn tasks_config() -> String {
    r#"
default_agent = "assistant"

[agents.assistant]
instructions = "你是助手（assistant 那一版身份）。"
tools = ["task", "read", "bash", "unsure", "gate"]

[channels.telegram]
enabled = true
allow_from = [111]
home_chat = 111

[model.main]
type = "completion"
api_backend = "responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[model.cheap]
type = "completion"
api_backend = "responses"
base_url = "https://llm.example.com/v1"
model = "gpt-cheap"
api_key_env = "KOMO_LLM_API_KEY"

[models]
default = "main"

[memory]
enabled = false
"#
    .to_string()
}

/// 按**这一段最后一条用户消息的第一行**分流的脚本模型：home 与任务会话是同一个 Agent，
/// 系统提示分不开它们，输入正文分得开（任务带契约时正文后面还补着契约段）。
///
/// - `帮我查一下空调`（home）→ 调 `task`（用 `cheap`、带契约），再把工具结果原样念出来；
/// - `功耗呢`（home）→ 调 `task` 接着做（目标短号由测试写进 `follow_task_id`，不给 model）；
/// - `跑个慢命令`（home）→ 调 `task`，派出去的任务会调 `unsure` 停在一条介入上；
/// - `派了再等`（home）→ 先调 `task`，再调 `gate` 停在门上，门开之后再说一句；
/// - `派个慢 bash`（home）→ 调 `task`，派出去的任务会跑 `echo step-1; sleep 30`；
/// - `进度呢`（home）→ 只带 `task_id`（写进 `follow_task_id`）调 `task` 查进度；`进度呢 zzzz`
///   查一个不存在的短号；
/// - `[后台任务 …]`（home，任务交回的结果）→ 回一句 `转告：<结果正文那一行>`；
/// - 任务会话收到的正文 → 各回一句固定的结论，或者调 `bash` / `unsure` / `task`。
struct TasksLlm {
    follow_task_id: Mutex<Option<String>>,
    requests: Mutex<Vec<TurnRequest>>,
}

impl TasksLlm {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            follow_task_id: Mutex::new(None),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn set_follow_target(&self, short_id: &str) {
        *self.follow_task_id.lock().expect("follow 目标") = Some(short_id.to_string());
    }
}

fn last_user_text(req: &TurnRequest) -> String {
    req.messages
        .iter()
        .rev()
        .find(|message| message.role == komo_kernel::types::turn::Role::User)
        .and_then(|message| message.text.clone())
        .unwrap_or_default()
}

/// 最后一条用户消息之后已经有工具结果了（这一段是接着上一段跑的）。
fn answered_since_last_user(req: &TurnRequest) -> bool {
    use komo_kernel::types::turn::Role;
    req.messages
        .iter()
        .rev()
        .take_while(|message| message.role != Role::User)
        .any(|message| message.role == Role::Tool)
}

#[async_trait::async_trait]
impl LlmClient for TasksLlm {
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
        self.requests.lock().expect("请求").push(req.clone());
        let input = last_user_text(&req);
        // fork 任务的输入开头是那段说明（`FORK_NOTICE`），要它做的事在最后一行。
        let input = if input.starts_with("（以上是派你的那个会话") {
            input.lines().last().unwrap_or_default().to_string()
        } else {
            input
        };
        let script = if input.starts_with("[后台任务") {
            let result = input.lines().nth(2).unwrap_or_default();
            Script::Reply(format!("转告：{result}"))
        } else {
            match input.lines().next().unwrap_or_default() {
                "帮我查一下空调" => Script::Calls(vec![(
                    "task",
                    serde_json::json!({
                        "prompt": "查一下空调状态",
                        "title": "查空调",
                        "model": "cheap",
                        "contract": { "type": "object", "required": ["on"] }
                    }),
                )]),
                "功耗呢" => {
                    let task_id = self
                        .follow_task_id
                        .lock()
                        .expect("follow 目标")
                        .clone()
                        .expect("follow 目标短号该在发这条消息之前写好");
                    Script::Calls(vec![(
                        "task",
                        serde_json::json!({ "task_id": task_id, "prompt": "再看看功耗" }),
                    )])
                }
                // 带 task_id 又带契约的追问：照旧排队，不插话（并入会丢掉新契约）。
                "功耗带契约" => {
                    let task_id = self
                        .follow_task_id
                        .lock()
                        .expect("follow 目标")
                        .clone()
                        .expect("follow 目标短号该在发这条消息之前写好");
                    Script::Calls(vec![(
                        "task",
                        serde_json::json!({
                            "task_id": task_id,
                            "prompt": "再看看功耗",
                            "contract": { "type": "object", "required": ["watts"] }
                        }),
                    )])
                }
                "等门" | "任务等门" => Script::Calls(vec![("gate", serde_json::json!({}))]),
                "派等门任务" => Script::Calls(vec![(
                    "task",
                    serde_json::json!({ "prompt": "任务等门", "title": "等门任务" }),
                )]),
                "追问等门任务" => {
                    let task_id = self
                        .follow_task_id
                        .lock()
                        .expect("follow 目标")
                        .clone()
                        .expect("follow 目标短号该在发这条消息之前写好");
                    Script::Calls(vec![(
                        "task",
                        serde_json::json!({ "task_id": task_id, "prompt": "任务补一句" }),
                    )])
                }
                "跑个慢命令" => Script::Calls(vec![(
                    "task",
                    serde_json::json!({ "prompt": "跑一下 echo hi", "title": "慢命令" }),
                )]),
                "派了再等" => Script::Calls(vec![
                    (
                        "task",
                        serde_json::json!({ "prompt": "查一下热水器状态", "title": "查热水器" }),
                    ),
                    ("gate", serde_json::json!({})),
                ]),
                "派个慢 bash" => Script::Calls(vec![(
                    "task",
                    serde_json::json!({ "prompt": "跑慢 bash", "title": "慢 bash" }),
                )]),
                "跑慢 bash" => Script::Calls(vec![(
                    "bash",
                    serde_json::json!({ "command": "echo step-1; sleep 30" }),
                )]),
                "进度呢" => {
                    let task_id = self
                        .follow_task_id
                        .lock()
                        .expect("follow 目标")
                        .clone()
                        .expect("目标短号该在发这条消息之前写好");
                    Script::Calls(vec![("task", serde_json::json!({ "task_id": task_id }))])
                }
                "进度呢 zzzz" => {
                    Script::Calls(vec![("task", serde_json::json!({ "task_id": "zzzz" }))])
                }
                "先看一眼" => Script::Reply("看完了：B1 在 Drop.php 的 invokeDrop。".into()),
                "派个 fork 去修" => Script::Calls(vec![(
                    "task",
                    serde_json::json!({ "prompt": "修 B1", "title": "修 B1", "context": "fork" }),
                )]),
                "修 B1" => Script::Reply("B1 修好了。".into()),
                "派个会停的 fork" => Script::Calls(vec![(
                    "task",
                    serde_json::json!({ "prompt": "fork 里等人", "title": "等人", "context": "fork" }),
                )]),
                // 重启后接着跑的那一段已经带着 `unsure` 的结果：这回直接答。
                "fork 里等人" if answered_since_last_user(&req) => {
                    Script::Reply("等到了。".into())
                }
                "fork 里等人" => Script::Calls(vec![("unsure", serde_json::json!({}))]),
                "B1 测了吗" => Script::Reply("测过了。".into()),
                "fork 里再派" => Script::Calls(vec![(
                    "task",
                    serde_json::json!({ "prompt": "里面那层", "title": "里层" }),
                )]),
                "再派一层" => Script::Calls(vec![(
                    "task",
                    serde_json::json!({ "prompt": "里面那层", "title": "里层" }),
                )]),
                "跑一下 echo hi" => Script::Calls(vec![("unsure", serde_json::json!({}))]),
                "查一下空调状态" => Script::Reply("空调已经关掉了。".into()),
                "查一下热水器状态" => Script::Reply("热水器 60 度。".into()),
                "再看看功耗" => Script::Reply("功耗 300W。".into()),
                "1+1 等于几" => Script::Reply("2".into()),
                other => Script::Reply(format!("没见过这句：{other}")),
            }
        };
        Ok(Box::new(ScriptDriver { script, step: 0 }))
    }
}

enum Script {
    Reply(String),
    /// 一轮一个调用，按顺序；调用都收尾之后把最后一条工具结果原样念出来。
    Calls(Vec<(&'static str, serde_json::Value)>),
}

struct ScriptDriver {
    script: Script,
    step: usize,
}

#[async_trait::async_trait]
impl TurnDriver for ScriptDriver {
    async fn next(&mut self, input: RoundInput) -> Result<Round, LlmError> {
        self.step += 1;
        let round = self.step as u32;
        match &self.script {
            Script::Reply(text) => text_round(round, text),
            Script::Calls(calls) => match calls.get(self.step - 1) {
                Some((tool, args)) => call_round(round, &format!("pc-{tool}"), tool, args.clone()),
                // 原样念出工具结果：短号是运行时现生成的，脚本没法提前写死。
                None => {
                    let text = match input {
                        RoundInput::ToolResults { results, .. } => results
                            .first()
                            .map(|result| result.content.clone())
                            .unwrap_or_default(),
                        RoundInput::First => String::new(),
                    };
                    text_round(round, &text)
                }
            },
        }
    }

    fn usage(&self) -> TokenUsage {
        TokenUsage::default()
    }
}

/// 送到某个渠道对端的全部文本回复：Run 的最终回复（`Outbound::RunFinished`）与其余文本。
fn texts_to(sender: &MemSender, chat: &str) -> Vec<String> {
    sender
        .to_chat(chat)
        .into_iter()
        .filter_map(|message| match message.outbound {
            Outbound::Text { text } => Some(text),
            Outbound::RunFinished { summary, .. } => Some(summary),
            _ => None,
        })
        .collect()
}

async fn dm(gateway: &Gw, text: &str, key: &str) -> (SessionId, RunId) {
    let ack = gateway
        .dispatcher()
        .handle(inbound(
            ChannelPlatform::Telegram,
            "111",
            "111",
            text,
            key,
            true,
        ))
        .await
        .expect("Dispatcher 处理入站消息");
    let InboundAck::Queued { session, run } = ack else {
        panic!("{ack:?}")
    };
    (session, run)
}

async fn runs_of(gateway: &Gw, session: &SessionId) -> Vec<komo_store::repos::runs::RunRecord> {
    komo_store::repos::runs::list_for_session(&gateway.state().db, session)
        .await
        .expect("读得出会话的 Run")
}

/// 一个会话的 JSONL 里这条 Run 的 `run.accepted`。
async fn accepted_of(
    gateway: &Gw,
    session: &SessionId,
    run: &RunId,
) -> komo_kernel::events::RunAccepted {
    let paths = gateway.state().ledgers.paths_for(session);
    let (events, _) = komo_store::session_log::read_events(
        &paths,
        session,
        komo_kernel::types::ids::Seq::ZERO,
        0,
    )
    .await
    .expect("读得出会话");
    events
        .into_iter()
        .find_map(|event| match event.payload {
            komo_kernel::events::EventPayload::RunAccepted(body)
                if event.run.as_ref() == Some(run) =>
            {
                Some(body)
            }
            _ => None,
        })
        .expect("这条 Run 受理过")
}

/// 任务交回的结果在 home 里受理成的那条 Run（请求键 `task-result:{run}`）。交回发生在
/// 任务的看客收尾那一刻，是异步的，所以要等。
async fn wait_report(gateway: &Gw, home: &SessionId, task_run: &RunId) -> RunId {
    let key = format!("task-result:{task_run}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if let Some(record) = runs_of(gateway, home)
            .await
            .into_iter()
            .find(|record| record.request_key.as_str() == key)
        {
            return record.run;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("任务 {task_run} 的结果没有交回 home");
}

/// 端到端：DM → home 调 `task`、回"已派出 #短号" → 任务会话以 home 的 Agent、用 `task` 给的
/// 模型跑完 → 结果**不直接**投给渠道，而是作为一条内部输入交回 home（带契约复验结论、
/// 不带模型），由 home 转告 → 带 `task_id` 接着做，沿用那个任务上一条 Run 的模型，结果照样
/// 经 home 转告（§14 `task` 验收 ①④⑤⑥⑦）。
#[tokio::test]
async fn a_background_task_reports_back_through_home() {
    let home = Home::with_config(&tasks_config());
    let llm = TasksLlm::new();
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gateway = home
        .start_with(Arc::clone(&llm) as Arc<dyn LlmClient>, Arc::clone(&sender))
        .await;

    // ── 1. home 一轮就收尾，回一句已派出。
    let (home_session, home_run) = dm(&gateway, "帮我查一下空调", "dm-1").await;
    let detail = gateway.wait_terminal(&home_run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    // 脚本模型原样念出的是投影过的工具结果：第一行是抬头，"已派出"在正文那一行。
    // 回复由看客在终态事件之后投出去，比库里的终态晚一步，所以要等。
    eventually("home 的回复里说了已派出", || {
        texts_to(&sender, "111")
            .iter()
            .any(|text| text.lines().any(|line| line.starts_with("已派出 #")))
    })
    .await;

    // ── 2. 任务会话：kind = task、origin = task:{home}、与 home 同一个 Agent、标题写死。
    let sessions = komo_store::repos::session::list(&gateway.state().db, false)
        .await
        .expect("读会话列表");
    let task = sessions
        .iter()
        .find(|record| record.kind == komo_store::models::SessionKind::Task)
        .unwrap_or_else(|| panic!("该有一条任务会话：{sessions:?}"))
        .clone();
    assert_eq!(task.origin, format!("task:{home_session}"));
    assert_eq!(task.agent_id, "assistant");
    assert_eq!(task.title, "查空调");

    // ── 3. 任务跑完，结果交回 home，由 home 转告；任务自己的回复不直接投给渠道。
    let task_run = runs_of(&gateway, &task.session).await[0].run.clone();
    let done = gateway.wait_terminal(&task_run).await;
    assert_eq!(done.summary.state, RunState::Completed, "{done:?}");
    let report = wait_report(&gateway, &home_session, &task_run).await;
    let relayed = gateway.wait_terminal(&report).await;
    assert_eq!(relayed.summary.state, RunState::Completed, "{relayed:?}");

    eventually("home 转告了任务的结果", || {
        texts_to(&sender, "111")
            .iter()
            .any(|text| text == "转告：空调已经关掉了。")
    })
    .await;
    let delivered = texts_to(&sender, "111");
    assert!(
        !delivered.iter().any(|text| text == "空调已经关掉了。"),
        "任务的原话不该直接投给渠道：{delivered:?}"
    );

    // ④ 任务 Run 的快照是 `task` 给的模型；交回的那条输入按 home 自己的默认。
    let task_record = runs_of(&gateway, &task.session).await.remove(0);
    assert_eq!(task_record.model.model, "gpt-cheap");
    let report_record = runs_of(&gateway, &home_session)
        .await
        .into_iter()
        .find(|record| record.run == report)
        .expect("交回的那条 Run");
    assert_eq!(report_record.model.model, "gpt-test");
    // ⑥ 带契约：任务的提示里有契约段，交回的输入里写着复验结论（不合契约也照样交回）。
    let task_request = llm
        .requests
        .lock()
        .expect("请求")
        .iter()
        .find(|request| request.session == task.session)
        .cloned()
        .expect("任务会话发过请求");
    assert!(
        last_user_text(&task_request).contains("（结果契约）"),
        "{:?}",
        task_request.messages
    );
    // ⑦ 任务会话的能力面里没有 `task`。
    assert!(
        !task_request
            .tools
            .iter()
            .any(|tool| tool.name == komo_agent::TASK_TOOL),
        "任务会话不该看得见 task"
    );
    let report_input = accepted_of(&gateway, &home_session, &report).await;
    assert!(report_input.interject, "交回的输入带插话标记");
    assert!(
        report_input
            .text
            .as_deref()
            .unwrap_or_default()
            .contains("结果契约复验：不符合"),
        "{:?}",
        report_input.text
    );

    // ── 4. 带 task_id 接着做：进同一个任务会话，沿用上一条 Run 的模型，结果照样经 home 转告。
    llm.set_follow_target(&komo_kernel::types::task::short_id(&task.session));
    let (_, home_run_2) = dm(&gateway, "功耗呢", "dm-2").await;
    gateway.wait_terminal(&home_run_2).await;
    let task_runs = runs_of(&gateway, &task.session).await;
    assert_eq!(task_runs.len(), 2, "{task_runs:?}");
    assert_eq!(task_runs[1].model.model, "gpt-cheap", "⑤ 沿用上一条的模型");
    // 派出去的第一条输入没有宿主可并，不带插话标记；不带契约的追问以插话受理。
    assert!(
        !accepted_of(&gateway, &task.session, &task_runs[0].run)
            .await
            .interject
    );
    assert!(
        accepted_of(&gateway, &task.session, &task_runs[1].run)
            .await
            .interject,
        "无契约的追问以插话受理"
    );
    let report_2 = wait_report(&gateway, &home_session, &task_runs[1].run).await;
    gateway.wait_terminal(&report_2).await;
    eventually("接着做的结果照样经 home 转告", || {
        texts_to(&sender, "111")
            .iter()
            .any(|text| text == "转告：功耗 300W。")
    })
    .await;

    // ── 5. 带契约的追问照旧排队：并入沿用宿主的快照，新契约会丢。
    let (_, home_run_3) = dm(&gateway, "功耗带契约", "dm-3").await;
    gateway.wait_terminal(&home_run_3).await;
    let task_runs = runs_of(&gateway, &task.session).await;
    assert_eq!(task_runs.len(), 3, "{task_runs:?}");
    assert!(
        !accepted_of(&gateway, &task.session, &task_runs[2].run)
            .await
            .interject,
        "带契约的追问不插话"
    );

    gateway.stop().await;
}

/// 渠道的普通消息插话（§6，2026-10-23）：home 忙着跑工具（停在一道门上）时说的第二句话，
/// 门开之后并进同一条 Run，不另开一条；被并走的那条是终态 `absorbed`，渠道收到一条
/// `RunAbsorbed`（飞书据此收尾它的"处理中"卡片），回答只由宿主给一次。
#[tokio::test]
async fn a_chat_message_is_absorbed_into_a_running_home_and_its_channel_is_told() {
    use komo_gateway::service::test_support::Gate;

    let home = Home::with_config(&tasks_config());
    let gate = Gate::new();
    home.install_tools(vec![gate.tool()]);
    let llm = TasksLlm::new();
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gateway = home
        .start_with(Arc::clone(&llm) as Arc<dyn LlmClient>, Arc::clone(&sender))
        .await;

    let (_, host) = dm(&gateway, "等门", "dm-1").await;
    gate.wait_entered(1).await;
    let (_, second) = dm(&gateway, "补一句", "dm-2").await;
    assert_eq!(
        gateway.db_state(&host).await,
        RunState::Running,
        "第二句到的时候 home 还在跑"
    );

    gate.release();
    assert_eq!(
        gateway.wait_terminal(&host).await.summary.state,
        RunState::Completed
    );
    assert_eq!(
        gateway.wait_terminal(&second).await.summary.state,
        RunState::Absorbed,
        "第二句并进了 home，没有另开一条"
    );

    eventually("渠道收到了并入通知", || {
        sender.to_chat("111").iter().any(|message| {
            matches!(&message.outbound, Outbound::RunAbsorbed { run, into, .. }
                if run == &second && into == &host)
        })
    })
    .await;
    // 回答只有宿主那一次：被并走的那条自己没有回复。
    eventually("宿主答了并进来的那句", || {
        texts_to(&sender, "111")
            .iter()
            .any(|text| text == "没见过这句：补一句")
    })
    .await;

    gateway.stop().await;
}

/// home 还在跑时交回的结果在一轮收尾后并进那条 Run（`run.absorbed`），不另开一条
/// （§6 插话，§14 `task` 验收 ③）：home 派出任务后停在一道门上，任务跑完、结果交回时 home
/// 还在跑；门开之后那一轮收尾，结果并进来，home 接着把它转告出去。
#[tokio::test]
async fn a_task_result_is_absorbed_into_a_running_home() {
    use komo_gateway::service::test_support::Gate;

    let home = Home::with_config(&tasks_config());
    let gate = Gate::new();
    home.install_tools(vec![gate.tool()]);
    let llm = TasksLlm::new();
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gateway = home
        .start_with(Arc::clone(&llm) as Arc<dyn LlmClient>, Arc::clone(&sender))
        .await;

    let (home_session, home_run) = dm(&gateway, "派了再等", "dm-1").await;
    let task_session = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let sessions = komo_store::repos::session::list(&gateway.state().db, false)
                .await
                .expect("读会话列表");
            if let Some(task) = sessions
                .into_iter()
                .find(|record| record.kind == komo_store::models::SessionKind::Task)
            {
                break task.session;
            }
            assert!(std::time::Instant::now() < deadline, "任务一直没派出去");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    let task_run = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if let Some(record) = runs_of(&gateway, &task_session).await.into_iter().next() {
                break record.run;
            }
            assert!(std::time::Instant::now() < deadline, "任务会话里没有 Run");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    gateway.wait_terminal(&task_run).await;
    let report = wait_report(&gateway, &home_session, &task_run).await;
    assert_eq!(
        gateway.db_state(&home_run).await,
        RunState::Running,
        "交回时 home 还在跑"
    );

    gate.release();
    let detail = gateway.wait_terminal(&home_run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    assert_eq!(
        gateway.wait_terminal(&report).await.summary.state,
        RunState::Absorbed,
        "交回的那条并进了 home，没有另开一条"
    );
    eventually("home 把并进来的结果转告了", || {
        texts_to(&sender, "111")
            .iter()
            .any(|text| text == "转告：热水器 60 度。")
    })
    .await;

    gateway.stop().await;
}

/// 任务正忙时的追问（无契约）并进它正在跑的那条 Run：被并走的追问是终态 `absorbed`，
/// 结果只由宿主那一次交回 home，不会为追问另交一份（§4、§6，2026-10-23）。
#[tokio::test]
async fn a_follow_up_absorbed_into_a_busy_task_reports_back_once() {
    use komo_gateway::service::test_support::Gate;

    let home = Home::with_config(&tasks_config());
    let gate = Gate::new();
    home.install_tools(vec![gate.tool()]);
    let llm = TasksLlm::new();
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gateway = home
        .start_with(Arc::clone(&llm) as Arc<dyn LlmClient>, Arc::clone(&sender))
        .await;

    let (home_session, first) = dm(&gateway, "派等门任务", "dm-1").await;
    gateway.wait_terminal(&first).await;
    gate.wait_entered(1).await;
    let task = komo_store::repos::session::list(&gateway.state().db, false)
        .await
        .expect("读会话列表")
        .into_iter()
        .find(|record| record.kind == komo_store::models::SessionKind::Task)
        .expect("派出了一个任务");
    let host = runs_of(&gateway, &task.session).await[0].run.clone();
    assert_eq!(gateway.db_state(&host).await, RunState::Running);

    llm.set_follow_target(&komo_kernel::types::task::short_id(&task.session));
    let (_, follow) = dm(&gateway, "追问等门任务", "dm-2").await;
    gateway.wait_terminal(&follow).await;
    let task_runs = runs_of(&gateway, &task.session).await;
    assert_eq!(task_runs.len(), 2, "{task_runs:?}");
    let follow_up = task_runs[1].run.clone();

    gate.release();
    assert_eq!(
        gateway.wait_terminal(&host).await.summary.state,
        RunState::Completed
    );
    assert_eq!(
        gateway.wait_terminal(&follow_up).await.summary.state,
        RunState::Absorbed,
        "追问并进了正在跑的任务，没有另开一条"
    );
    eventually("宿主那一次把结果交回并转告", || {
        texts_to(&sender, "111")
            .iter()
            .any(|text| text == "转告：没见过这句：任务补一句")
    })
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let reports = runs_of(&gateway, &home_session)
        .await
        .into_iter()
        .filter(|record| record.request_key.as_str().starts_with("task-result:"))
        .collect::<Vec<_>>();
    assert_eq!(reports.len(), 1, "只有宿主那一次交回：{reports:?}");
    assert_eq!(
        reports[0].request_key.as_str(),
        format!("task-result:{host}"),
        "被并走的追问不另交回"
    );

    gateway.stop().await;
}

/// 任务会话里调不动 `task`：能力面里没有它（§14 `task` 验收 ⑦）。
#[tokio::test]
async fn a_task_session_cannot_spawn_another_task() {
    let home = Home::with_config(&tasks_config());
    let llm = TasksLlm::new();
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gateway = home
        .start_with(Arc::clone(&llm) as Arc<dyn LlmClient>, Arc::clone(&sender))
        .await;

    // 先让 home 派出一个任务、等它跑完，再往那个任务会话里说"再派一层"。
    let (_, home_run) = dm(&gateway, "帮我查一下空调", "dm-1").await;
    gateway.wait_terminal(&home_run).await;
    let task = komo_store::repos::session::list(&gateway.state().db, false)
        .await
        .expect("读会话列表")
        .into_iter()
        .find(|record| record.kind == komo_store::models::SessionKind::Task)
        .expect("派出了一个任务");
    let first = runs_of(&gateway, &task.session).await[0].run.clone();
    gateway.wait_terminal(&first).await;
    let inner = gateway
        .submit(&task.session, "inner-1", "再派一层")
        .await
        .run;
    gateway.wait_terminal(&inner).await;

    let sessions = komo_store::repos::session::list(&gateway.state().db, false)
        .await
        .expect("读会话列表");
    assert_eq!(
        sessions
            .iter()
            .filter(|record| record.kind == komo_store::models::SessionKind::Task)
            .count(),
        1,
        "任务会话里没有再派出任务"
    );
    let detail = gateway.wait_terminal(&inner).await;
    assert!(
        detail
            .calls
            .iter()
            .all(|call| call.state == komo_kernel::types::status::ToolCallState::Failed),
        "{detail:?}"
    );

    gateway.stop().await;
}

/// `context: "fork"`（`docs/background-tasks.md` §2b）：任务带着 home 此刻的对话起步——
/// 首请求的系统提示、工具定义与 home 上一次请求相同，消息以 home 上一次请求的消息为前缀，
/// 之后才是派它的那次调用、补的结果与任务；缓存按 home 路由；模型沿用 home 的。fork 里的
/// 追问照样带着前缀，即使 home 的目录已经不在；fork 里不能再派任务。
#[tokio::test]
async fn a_fork_starts_from_the_parents_conversation_and_shares_its_prefix() {
    let home = Home::with_config(&tasks_config());
    let llm = TasksLlm::new();
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gateway = home
        .start_with(Arc::clone(&llm) as Arc<dyn LlmClient>, Arc::clone(&sender))
        .await;

    let (home_session, first) = dm(&gateway, "先看一眼", "dm-1").await;
    gateway.wait_terminal(&first).await;
    let (_, forking) = dm(&gateway, "派个 fork 去修", "dm-2").await;
    gateway.wait_terminal(&forking).await;

    let task = komo_store::repos::session::list(&gateway.state().db, false)
        .await
        .expect("读会话列表")
        .into_iter()
        .find(|record| record.kind == komo_store::models::SessionKind::Task)
        .expect("派出了一个 fork");
    let task_run = runs_of(&gateway, &task.session).await[0].run.clone();
    let done = gateway.wait_terminal(&task_run).await;
    assert_eq!(done.summary.state, RunState::Completed, "{done:?}");

    let requests = llm.requests.lock().expect("请求").clone();
    let parent = requests
        .iter()
        .find(|request| {
            request.session == home_session && last_user_text(request) == "派个 fork 去修"
        })
        .cloned()
        .expect("home 派任务的那次请求");
    let fork = requests
        .iter()
        .find(|request| request.session == task.session)
        .cloned()
        .expect("fork 发过请求");

    // ① 同一个前缀：系统提示、工具定义逐字相同，消息以 home 上一次请求的消息开头。
    assert_eq!(fork.system_prompt, parent.system_prompt);
    assert_eq!(fork.tools, parent.tools, "fork 的工具定义与父相同，含 task");
    assert!(
        fork.tools
            .iter()
            .any(|tool| tool.name == komo_agent::TASK_TOOL)
    );
    let n = parent.messages.len();
    // `seq` indexes the source Session's ledger; the fork's tree-mode context zeroes it
    // so it can never collide with this Session's own compaction cut points. It is never
    // sent to a provider, so the shared prefix is compared by content.
    let unsequenced = |messages: &[komo_kernel::types::turn::ReplayMessage]| {
        messages
            .iter()
            .cloned()
            .map(|mut message| {
                message.seq = komo_kernel::types::ids::Seq::ZERO;
                message
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        unsequenced(&fork.messages[..n]),
        unsequenced(&parent.messages[..])
    );
    use komo_kernel::types::turn::Role;
    let call = &fork.messages[n];
    assert_eq!(call.role, Role::Assistant);
    assert_eq!(call.tool_calls[0].name, "task");
    let closed = &fork.messages[n + 1];
    assert_eq!(closed.role, Role::Tool);
    assert_eq!(closed.tool_results[0].call_id, call.tool_calls[0].call_id);
    assert!(
        closed.tool_results[0].content.contains("派出的就是你"),
        "{closed:?}"
    );
    let own = fork.messages[n + 2].text.clone().unwrap_or_default();
    assert!(own.starts_with("（以上是派你的那个会话"), "{own}");
    assert!(own.ends_with("\n\n修 B1"), "{own}");
    assert_eq!(
        fork.cache_session.as_ref(),
        Some(&home_session),
        "缓存按父会话路由"
    );
    assert_eq!(fork.model.model, parent.model.model, "fork 沿用父的模型");

    // 结果照常经 home 转告。
    let report = wait_report(&gateway, &home_session, &task_run).await;
    gateway.wait_terminal(&report).await;
    eventually("home 转告了 fork 的结果", || {
        texts_to(&sender, "111")
            .iter()
            .any(|text| text == "转告：B1 修好了。")
    })
    .await;

    // ② fork 里看得见 task，但派不出去：调用落一条失败结果，没有新的任务会话。
    let inner = gateway
        .submit(&task.session, "inner-1", "fork 里再派")
        .await
        .run;
    let detail = gateway.wait_terminal(&inner).await;
    assert!(
        detail
            .calls
            .iter()
            .all(|call| call.state == komo_kernel::types::status::ToolCallState::Failed),
        "{detail:?}"
    );
    let tasks = komo_store::repos::session::list(&gateway.state().db, false)
        .await
        .expect("读会话列表")
        .into_iter()
        .filter(|record| record.kind == komo_store::models::SessionKind::Task)
        .count();
    assert_eq!(tasks, 1, "fork 里没有再派出任务");

    // ④ fork 只读自己的目录：把 home 的目录挪走（放在最后，挪走之后 home 自己跑不了），
    // 追问照样带着前缀跑完。
    let home_dir = gateway
        .state()
        .ledgers
        .paths_for(&home_session)
        .root()
        .to_path_buf();
    let moved = home_dir.with_extension("moved");
    std::fs::rename(&home_dir, &moved).expect("挪走 home 的目录");
    let follow = gateway
        .submit(&task.session, "follow-1", "B1 测了吗")
        .await
        .run;
    let followed = gateway.wait_terminal(&follow).await;
    assert_eq!(followed.summary.state, RunState::Completed, "{followed:?}");
    let follow_request = llm
        .requests
        .lock()
        .expect("请求")
        .iter()
        .rev()
        .find(|request| request.session == task.session)
        .cloned()
        .expect("追问发过请求");
    assert_eq!(follow_request.system_prompt, parent.system_prompt);
    assert_eq!(
        unsequenced(&follow_request.messages[..n]),
        unsequenced(&parent.messages[..])
    );
    assert_eq!(follow_request.tools, parent.tools, "追问沿用 fork 的能力面");

    gateway.stop().await;
}

/// fork 停在一条介入上、Gateway 重启之后接着跑：恢复那一段的请求以重启前那次请求为
/// 逐字节前缀——冻结的前缀从任务会话自己的目录读回，系统提示、工具与缓存键都不变（§2b）。
#[tokio::test]
async fn a_fork_resumed_after_a_restart_sends_the_same_prefix() {
    let home = Home::with_config(&tasks_config());
    home.install_tools(vec![Unsure::new().tool()]);
    let llm = TasksLlm::new();
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gateway = home
        .start_with(Arc::clone(&llm) as Arc<dyn LlmClient>, Arc::clone(&sender))
        .await;

    let (_, home_run) = dm(&gateway, "派个会停的 fork", "dm-1").await;
    gateway.wait_terminal(&home_run).await;
    let task_run = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if let Some(run) = gateway
                .interventions()
                .await
                .into_iter()
                .find_map(|one| one.run)
            {
                break run;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fork 一直没停在介入上"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };
    let pending = gateway.wait_intervention(&task_run).await;
    let task_session = komo_store::repos::runs::get(&gateway.state().db, &task_run)
        .await
        .expect("读得出 fork 的 Run")
        .expect("fork 的 Run 在账本里")
        .session;
    gateway.stop().await;

    let resumed_llm = TasksLlm::new();
    let gateway = home
        .start_with(
            Arc::clone(&resumed_llm) as Arc<dyn LlmClient>,
            Arc::clone(&sender),
        )
        .await;
    gateway.answer(&pending.handle, "satisfied").await;
    let detail = gateway.wait_terminal(&task_run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let of_task = |llm: &TasksLlm| {
        llm.requests
            .lock()
            .expect("请求")
            .iter()
            .find(|request| request.session == task_session)
            .cloned()
            .expect("fork 发过请求")
    };
    let before = of_task(&llm);
    let after = of_task(&resumed_llm);
    assert_eq!(after.system_prompt, before.system_prompt);
    assert_eq!(after.tools, before.tools);
    assert_eq!(after.cache_session, before.cache_session);
    assert!(
        after.messages.len() > before.messages.len(),
        "恢复那一段多了调用与结果"
    );
    assert_eq!(after.messages[..before.messages.len()], before.messages[..]);

    gateway.stop().await;
}

/// 任务停在一条要人答的介入上时（结果不明，§7.5），home 照样几秒内答完下一句——任务在
/// 自己的会话里排队，不占 home 的串行位。
#[tokio::test]
async fn home_answers_while_a_task_waits_on_a_person() {
    let home = Home::with_config(&tasks_config());
    home.install_tools(vec![Unsure::new().tool()]);
    let llm = TasksLlm::new();
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gateway = home
        .start_with(Arc::clone(&llm) as Arc<dyn LlmClient>, Arc::clone(&sender))
        .await;

    let (_, home_run) = dm(&gateway, "跑个慢命令", "dm-1").await;
    gateway.wait_terminal(&home_run).await;
    let task_run = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if let Some(run) = gateway
                .interventions()
                .await
                .into_iter()
                .find_map(|one| one.run)
            {
                break run;
            }
            assert!(std::time::Instant::now() < deadline, "任务一直没停在介入上");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };
    assert!(
        matches!(
            gateway.wait_waiting(&task_run).await,
            WaitReason::Intervention { .. }
        ),
        "任务停在一条介入上"
    );

    let (_, home_run_2) = dm(&gateway, "1+1 等于几", "dm-2").await;
    let detail = gateway.wait_terminal(&home_run_2).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    assert_eq!(
        gateway.db_state(&task_run).await,
        RunState::Waiting,
        "home 答完的时候任务还停在介入上"
    );

    gateway.stop().await;
}

/// 查进度（§14 task 进度查询验收）：任务正跑着一条 `bash`（已经打出一行、还在 sleep），
/// home 只给 `task_id` 调 `task`——拿到的快照里有正在执行的调用、命令摘要和 `.partial`
/// 里的输出尾部；查询不往任务会话里提交任何输入（没有新的 Run）。
#[tokio::test]
async fn a_status_query_shows_what_a_running_task_is_doing() {
    let home = Home::with_config(&tasks_config());
    let llm = TasksLlm::new();
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gateway = home
        .start_with(Arc::clone(&llm) as Arc<dyn LlmClient>, Arc::clone(&sender))
        .await;

    let (_, home_run) = dm(&gateway, "派个慢 bash", "dm-1").await;
    gateway.wait_terminal(&home_run).await;
    let task = komo_store::repos::session::list(&gateway.state().db, false)
        .await
        .expect("读会话列表")
        .into_iter()
        .find(|record| record.kind == komo_store::models::SessionKind::Task)
        .expect("派出了一个任务");

    // 等 bash 的第一行输出落进 `.partial`：这时它还在 sleep。
    let tool_output = gateway
        .state()
        .ledgers
        .paths_for(&task.session)
        .tool_output();
    let partial_has_step_1 = || {
        let mut stack = vec![tool_output.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path
                    .file_name()
                    .is_some_and(|name| name == "stdout.txt.partial")
                    && std::fs::read_to_string(&path).is_ok_and(|text| text.contains("step-1"))
                {
                    return true;
                }
            }
        }
        false
    };
    eventually("bash 的 step-1 写进了 .partial", partial_has_step_1).await;
    let runs_before = runs_of(&gateway, &task.session).await;
    assert_eq!(runs_before.len(), 1, "{runs_before:?}");

    llm.set_follow_target(&komo_kernel::types::task::short_id(&task.session));
    let (_, ask) = dm(&gateway, "进度呢", "dm-2").await;
    gateway.wait_terminal(&ask).await;

    let replies = texts_to(&sender, "111");
    let snapshot = replies
        .iter()
        .find(|text| text.contains("正在执行：bash"))
        .unwrap_or_else(|| panic!("没有进度快照：{replies:?}"));
    assert!(snapshot.contains("echo step-1; sleep 30"), "{snapshot}");
    assert!(snapshot.contains("step-1"), "{snapshot}");
    assert_eq!(
        runs_of(&gateway, &task.session).await.len(),
        1,
        "查进度不提交任何输入"
    );

    gateway.stop().await;
}

/// 查一个不存在的短号：干净失败，开头是"查不到进度"。
#[tokio::test]
async fn a_status_query_for_an_unknown_task_fails_cleanly() {
    let home = Home::with_config(&tasks_config());
    let llm = TasksLlm::new();
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gateway = home
        .start_with(Arc::clone(&llm) as Arc<dyn LlmClient>, Arc::clone(&sender))
        .await;

    let (_, run) = dm(&gateway, "进度呢 zzzz", "dm-1").await;
    gateway.wait_terminal(&run).await;

    let replies = texts_to(&sender, "111");
    assert!(
        replies.iter().any(|text| text.contains("查不到进度")),
        "{replies:?}"
    );
    assert!(
        komo_store::repos::session::list(&gateway.state().db, false)
            .await
            .expect("读会话列表")
            .iter()
            .all(|record| record.kind != komo_store::models::SessionKind::Task),
        "查进度不建任务会话"
    );

    gateway.stop().await;
}
