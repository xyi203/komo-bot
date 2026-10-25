//! 长 `bash` 命令自动转后台（`docs/monitor.md` §8）：对话会话里跑过 `detach_after_secs`
//! 还没完的命令，这次调用立刻带着「已转后台 #id」收尾，进程留在 Gateway 手里，结束时交回一条
//! `bash-done:` 消息；`monitor_stop` 停得掉它；超时在后台照样生效；Gateway 重启时没交回的
//! 那条补一句"结果不明"。
//!
//! 真数据目录、真 `service::start`、真子进程；模型是按输入分流的脚本。

use std::sync::{Arc, Mutex};

use komo_gateway::service::test_support::harness::{call_round, text_round};
use komo_kernel::traits::{LlmClient, TurnDriver};
use komo_kernel::types::model::TokenUsage;
use komo_kernel::types::turn::{LlmError, Round, RoundInput, TurnRequest};

mod durable;

fn config(detach_after_secs: u64) -> String {
    format!(
        r#"
default_agent = "assistant"

[agents.assistant]
instructions = "你是助手。"
tools = ["monitor_stop", "bash"]

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

[execution]
detach_after_secs = {detach_after_secs}
"#
    )
}

/// 按这一段最后一条用户输入分流：
///
/// - `跑：<命令>` / `限时跑：<命令>` → 调 `bash`（后者 `timeout_secs = 3`），再念出工具结果；
/// - `停：<短号>` → 调 `monitor_stop`；
/// - `[后台命令 …` → 记下，回一句"收到"；
/// - 其他 → 原样回一句。
struct DetachLlm {
    inputs: Mutex<Vec<String>>,
}

impl DetachLlm {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inputs: Mutex::new(Vec::new()),
        })
    }

    fn handbacks(&self) -> Vec<String> {
        self.inputs.lock().unwrap().clone()
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
impl LlmClient for DetachLlm {
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
        let input = last_user_text(&req);
        let script = if input.starts_with("[后台命令") {
            self.inputs.lock().unwrap().push(input);
            Script::Reply("收到".into())
        } else if let Some(command) = input.strip_prefix("限时跑：") {
            Script::Call(
                "bash",
                serde_json::json!({ "command": command, "timeout_secs": 3 }),
            )
        } else if let Some(command) = input.strip_prefix("跑：") {
            Script::Call("bash", serde_json::json!({ "command": command }))
        } else if let Some(id) = input.strip_prefix("停：") {
            Script::Call("monitor_stop", serde_json::json!({ "monitor_id": id }))
        } else {
            Script::Reply(input)
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
