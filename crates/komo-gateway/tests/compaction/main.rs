//! §6 的在线压缩，端到端：真数据目录、真 `service::start`、脚本化模型。
//!
//! - **窗口保护**：`context_window` 调得很小，几次大 `read` 把估计的上下文推过线——摘要
//!   请求没有工具；同一个 Run 接着跑，下一次主请求是任务 → 摘要 → 切点起原样；中途停在
//!   一条介入上、重启再答，回放的那一段逐字节不变。
//! - **不贴窗口不压**：开着压缩、短任务——不多发任何请求，driver 原样接着跑，模型拿到的
//!   轮输入与关着压缩时一模一样，日志里没有 `context.compacted`。
//!
//! 断言的是**模型收到了什么**（`FakeLlm` 记下的请求与轮输入）和日志里的 `context.compacted`。

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

/// 一次请求的估价：与 Gateway 判断时同一把尺子（字节 / 4）——系统提示、工具表、每条消息。
fn priced(request: &TurnRequest) -> u64 {
    use komo_kernel::compaction::{IMAGE_TOKENS, estimate_tokens};
    let tools: u64 = request
        .tools
        .iter()
        .map(|tool| {
            estimate_tokens(&tool.name)
                + estimate_tokens(&tool.description)
                + estimate_tokens(&tool.parameters.to_string())
        })
        .sum();
    let messages: u64 = request
        .messages
        .iter()
        .map(|message| {
            let body = match &message.provider_blocks {
                Some(blocks) => estimate_tokens(&blocks.to_string()),
                None => {
                    message.text.as_deref().map_or(0, estimate_tokens)
                        + message
                            .tool_calls
                            .iter()
                            .map(|call| {
                                estimate_tokens(&call.name)
                                    + estimate_tokens(&call.arguments.to_string())
                            })
                            .sum::<u64>()
                }
            };
            body + message
                .tool_results
                .iter()
                .map(|r| estimate_tokens(&r.content) + IMAGE_TOKENS * r.images.len() as u64)
                .sum::<u64>()
        })
        .sum();
    estimate_tokens(&request.system_prompt) + tools + messages
}

mod durable;
