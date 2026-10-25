//! 监听（`docs/monitor.md`）：`monitor` 挂上的脚本每出一行，就作为一条内部输入交回挂它的
//! 会话；脚本收尾时交回一条结束消息；`monitor_stop` 停掉它；Gateway 重启时监听中断并
//! 告诉会话，不自动重挂——除非挂的时候声明了 `resume_after_restart`。
//!
//! 真数据目录、真 `service::start`、真子进程；模型是按输入分流的脚本。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use komo_gateway::service::test_support::harness::{call_round, text_round};
use komo_kernel::traits::{LlmClient, TurnDriver};
use komo_kernel::types::model::TokenUsage;
use komo_kernel::types::turn::{LlmError, Round, RoundInput, TurnRequest};

mod durable;

fn config() -> String {
    r#"
default_agent = "assistant"

[agents.assistant]
instructions = "你是助手。"
tools = ["monitor", "monitor_stop", "bash"]

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

/// 按这一段最后一条用户输入分流：
///
/// - `挂：<命令>` → 调 `monitor`，再把工具结果原样念出来；
/// - `停：<短号>` → 调 `monitor_stop`；
/// - `[监听 …` → 记下这条输入，回一句"收到"（`event_delay` 模拟模型处理得慢）；
/// - 其他 → 原样回一句。
struct MonitorLlm {
    inputs: Mutex<Vec<String>>,
    event_delay: Duration,
}

impl MonitorLlm {
    fn new(event_delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            inputs: Mutex::new(Vec::new()),
            event_delay,
        })
    }

    fn events(&self) -> Vec<String> {
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
impl LlmClient for MonitorLlm {
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
        let input = last_user_text(&req);
        let script = if input.starts_with("[监听") {
            self.inputs.lock().unwrap().push(input);
            tokio::time::sleep(self.event_delay).await;
            Script::Reply("收到".into())
        } else if let Some(command) = input.strip_prefix("续挂：") {
            Script::Call(
                "monitor",
                serde_json::json!({
                    "command": command,
                    "title": "测试监听",
                    "timeout_minutes": 2,
                    "resume_after_restart": true
                }),
            )
        } else if let Some(command) = input.strip_prefix("挂：") {
            Script::Call(
                "monitor",
                serde_json::json!({ "command": command, "title": "测试监听", "timeout_minutes": 2 }),
            )
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
