//! §6 的在线压缩，端到端：真数据目录、真 `service::start`、脚本化模型。
//!
//! - **窗口保护**：`context_window` 调得很小，几次大 `read` 把估计的上下文推过线——摘要
//!   请求没有工具；同一个 Run 接着跑，下一次主请求是任务 → 摘要 → 切点起原样；中途停在
//!   一条介入上、重启再答，回放的那一段逐字节不变。
//! - **不贴窗口不压**：开着压缩、短任务——不多发任何请求，driver 原样接着跑，模型拿到的
//!   轮输入与关着压缩时一模一样，日志里没有 `context.compacted`。
//!
//! 断言的是**模型收到了什么**（`FakeLlm` 记下的请求与轮输入）和日志里的 `context.compacted`。

use komo_agent::context::compaction::price;
use komo_gateway::service::test_support::harness::{call_round, text_round};
use komo_kernel::events::{ContextCompacted, Event, EventPayload};
use komo_kernel::types::turn::{RoundInput, TurnRequest};

fn compactions(events: &[Event]) -> Vec<ContextCompacted> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ContextCompacted(body) => Some(body.clone()),
            _ => None,
        })
        .collect()
}

/// 模型拿到的每一份轮输入，去掉运行时自己发的 `call_id` 与抬头里的耗时：两台 Gateway
/// 之间可比。
fn fed(inputs: &[RoundInput]) -> Vec<Option<Vec<(String, String, bool)>>> {
    inputs
        .iter()
        .map(|input| match input {
            RoundInput::First => None,
            RoundInput::ToolResults { results, revised } => {
                assert!(revised.is_empty());
                Some(
                    results
                        .iter()
                        .map(|r| {
                            (
                                r.provider_call_id.clone(),
                                without_elapsed(&r.content),
                                r.is_error,
                            )
                        })
                        .collect(),
                )
            }
        })
        .collect()
}

/// 抬头 `[工具 · 状态 · 耗时 …]` 的第三段是墙钟：两台 Gateway 各跑各的，负载下差几毫秒。
fn without_elapsed(content: &str) -> String {
    let Some((head, body)) = content.split_once('\n') else {
        return content.to_string();
    };
    let mut fields: Vec<&str> = head.split(" · ").collect();
    if let Some(elapsed) = fields.get_mut(2) {
        *elapsed = if elapsed.ends_with(']') {
            "…]"
        } else {
            "…"
        };
    }
    format!("{}\n{body}", fields.join(" · "))
}

/// 两条小命令 → 收尾。
fn short_script()
-> Vec<Vec<Result<komo_kernel::types::turn::Round, komo_kernel::types::turn::LlmError>>> {
    vec![vec![
        call_round(
            1,
            "pc-b1",
            "bash",
            serde_json::json!({ "command": "echo a" }),
        ),
        call_round(
            2,
            "pc-b2",
            "bash",
            serde_json::json!({ "command": "echo b" }),
        ),
        text_round(3, "都做完了。"),
    ]]
}

/// 一次请求的估价：与 Gateway 判断时同一把尺子。
fn priced(request: &TurnRequest) -> u64 {
    let context = komo_agent::context::AgentContext {
        system_prompt: request.system_prompt.clone(),
        messages: request.messages.clone(),
        run_from: 0,
        decay_candidates: Vec::new(),
    };
    price(&context, &request.tools).write_tokens
}

mod durable;
