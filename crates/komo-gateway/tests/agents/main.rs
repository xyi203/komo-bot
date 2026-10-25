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

use komo_gateway::service::test_support::harness::{FakeLlm, call_round, text_round};
use komo_kernel::traits::{LlmClient, TurnDriver};
use komo_kernel::types::model::TokenUsage;
use komo_kernel::types::turn::{LlmError, Round, RoundInput, TurnRequest};

mod durable;
mod durable_profile;

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

// ---------------------------------------------------------------- 在飞的 Run 不受 Profile 改动影响

// ---------------------------------------------------------------- 项目规范同样受理时冻结

// ---------------------------------------------------------------- 重启之后仍然用它自己那一版

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
