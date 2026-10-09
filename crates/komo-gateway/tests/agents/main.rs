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

use komo_gateway::service::test_support::harness::{
    DEFAULT_ENV, FakeLlm, Gw, Home, MemSender, call_round, inbound, text_round, write_home_with,
};
use komo_kernel::protocol::InboundAck;
use komo_kernel::traits::{Inbound, LlmClient, TurnDriver};
use komo_kernel::types::chat::{ChannelPlatform, Outbound};
use komo_kernel::types::ids::{RunId, SessionId};
use komo_kernel::types::model::TokenUsage;
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

// ---------------------------------------------------------------- 后台任务（`docs/background-tasks.md`）

/// home 用的就是它自己的 Agent：能派后台任务（`dispatch` / `follow`），也能自己动手。
/// 渠道是 Telegram（操作者 111，home chat 也是 111），DM 走真 Dispatcher，不绕过它。
fn tasks_config() -> String {
    r#"
default_agent = "assistant"

[agents.assistant]
instructions = "你是助手（assistant 那一版身份）。"
tools = ["dispatch", "follow", "read", "shell"]

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

[models]
default = "main"

[memory]
enabled = false
"#
    .to_string()
}

/// 按**这一段最后一条用户消息**分流的脚本模型：home 与任务会话是同一个 Agent，系统
/// 提示分不开它们，输入正文分得开。
///
/// - `帮我查一下空调`（home）→ 调 `dispatch`，再把工具结果原样念出来；
/// - `功耗呢`（home）→ 调 `follow`（目标短号由测试写进 `follow_task_id`）；
/// - `跑个慢命令`（home）→ 调 `dispatch`，派出去的任务会调 `shell` 停在审批上；
/// - `[后台任务 …]`（home，任务交回的结果）→ 回一句 `转告：<结果正文那一行>`；
/// - 任务会话收到的正文 → 各回一句固定的结论，或者调 `shell`。
struct TasksLlm {
    follow_task_id: Mutex<Option<String>>,
}

impl TasksLlm {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            follow_task_id: Mutex::new(None),
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

#[async_trait::async_trait]
impl LlmClient for TasksLlm {
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
        let input = last_user_text(&req);
        let script = if input.starts_with("[后台任务") {
            let result = input.lines().nth(2).unwrap_or_default();
            Script::Reply(format!("转告：{result}"))
        } else {
            match input.as_str() {
                "帮我查一下空调" => Script::Call(
                    "dispatch",
                    serde_json::json!({ "task": "查一下空调状态", "title": "查空调" }),
                ),
                "功耗呢" => {
                    let task_id = self
                        .follow_task_id
                        .lock()
                        .expect("follow 目标")
                        .clone()
                        .expect("follow 目标短号该在发这条消息之前写好");
                    Script::Call(
                        "follow",
                        serde_json::json!({ "task_id": task_id, "text": "再看看功耗" }),
                    )
                }
                "跑个慢命令" => Script::Call(
                    "dispatch",
                    serde_json::json!({ "task": "跑一下 echo hi", "title": "慢命令" }),
                ),
                "跑一下 echo hi" => {
                    Script::Call("shell", serde_json::json!({ "command": "echo hi" }))
                }
                "查一下空调状态" => Script::Reply("空调已经关掉了。".into()),
                "再看看功耗" => Script::Reply("功耗 300W。".into()),
                "1+1 等于几" => Script::Reply("2".into()),
                other => Script::Reply(format!("没见过这句：{other}")),
            }
        };
        Ok(Box::new(ScriptDriver { script }))
    }
}

enum Script {
    Reply(String),
    Call(&'static str, serde_json::Value),
}

struct ScriptDriver {
    script: Script,
}

#[async_trait::async_trait]
impl TurnDriver for ScriptDriver {
    async fn next(&mut self, input: RoundInput) -> Result<Round, LlmError> {
        match (&self.script, input) {
            (Script::Reply(text), _) => text_round(1, text),
            (Script::Call(tool, args), RoundInput::First) => {
                call_round(1, &format!("pc-{tool}"), tool, args.clone())
            }
            // 原样念出工具结果：短号是运行时现生成的，脚本没法提前写死。
            (Script::Call(..), RoundInput::ToolResults { results, .. }) => {
                let text = results
                    .first()
                    .map(|result| result.content.clone())
                    .unwrap_or_default();
                text_round(2, &text)
            }
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

/// 端到端：DM → home 调 `dispatch`、回"已派出 #短号" → 任务会话以 home 的 Agent 跑完 →
/// 结果**不直接**投给渠道，而是作为一条内部输入交回 home，由 home 转告 → `follow` 进同
/// 一个任务会话，结果照样经 home 转告。
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
    let delivered = texts_to(&sender, "111");
    assert!(
        delivered
            .iter()
            .any(|text| text.lines().any(|line| line.starts_with("已派出 #"))),
        "{delivered:?}"
    );

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

    let delivered = texts_to(&sender, "111");
    assert!(
        delivered
            .iter()
            .any(|text| text == "转告：空调已经关掉了。"),
        "{delivered:?}"
    );
    assert!(
        !delivered.iter().any(|text| text == "空调已经关掉了。"),
        "任务的原话不该直接投给渠道：{delivered:?}"
    );

    // ── 4. follow 进同一个任务会话，结果照样经 home 转告。
    llm.set_follow_target(&komo_kernel::types::task::short_id(&task.session));
    let (_, home_run_2) = dm(&gateway, "功耗呢", "dm-2").await;
    gateway.wait_terminal(&home_run_2).await;
    let task_runs = runs_of(&gateway, &task.session).await;
    assert_eq!(task_runs.len(), 2, "{task_runs:?}");
    let report_2 = wait_report(&gateway, &home_session, &task_runs[1].run).await;
    gateway.wait_terminal(&report_2).await;
    assert!(
        texts_to(&sender, "111")
            .iter()
            .any(|text| text == "转告：功耗 300W。"),
        "{:?}",
        texts_to(&sender, "111")
    );

    gateway.stop().await;
}

/// 任务卡在 shell 审批上时，home 照样几秒内答完下一句——任务在自己的会话里排队，
/// 不占 home 的串行位。
#[tokio::test]
async fn home_answers_while_a_task_waits_on_approval() {
    let home = Home::with_config(&tasks_config());
    let llm = TasksLlm::new();
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gateway = home
        .start_with(Arc::clone(&llm) as Arc<dyn LlmClient>, Arc::clone(&sender))
        .await;

    let (_, home_run) = dm(&gateway, "跑个慢命令", "dm-1").await;
    gateway.wait_terminal(&home_run).await;
    let approval = gateway.wait_approval().await;
    let task_run = approval.run.clone().expect("审批挂在任务的 Run 上");
    assert_eq!(gateway.db_state(&task_run).await, RunState::Waiting);

    let (_, home_run_2) = dm(&gateway, "1+1 等于几", "dm-2").await;
    let detail = gateway.wait_terminal(&home_run_2).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    assert_eq!(
        gateway.db_state(&task_run).await,
        RunState::Waiting,
        "home 答完的时候任务还停在审批上"
    );

    gateway.stop().await;
}
