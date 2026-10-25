//! §8.3 的「工具结果正文」与 `docs/komo_observation.md` 的 M1–M3，端到端两个测试：
//!
//! - **投影**：超预算时头尾都留着、写明省了多少，并给出**可直接 `read` 的资源入口**
//!   （`artifact://<run>/<call>/<attempt>/result`，§4.7）；
//! - **恢复**：那条入口真的读得进去——Session 自己的输出是只读根，不停下来问人；
//! - **预算**：`[execution] model_result_bytes` 说了算（§6），不是写死的 1 KiB；
//! - **一份事实**：同一个 Run 的下一个执行段回放时，渲染出的字节与刚跑完那次**完全相同**
//!   ——跑过的调用如此，没跑起来的（工具名不认识）也如此。
//! - **组合调用**：`edit` 带 `then_run` 两步各跑一次，结果是一条，回放逐字节相同。
//! - **压缩**：账本里一条 `context.compacted` 之后，回放是任务 → 摘要 → 切点起的原样轮次，
//!   外置的摘要按引用读回来，两次重启之间那一段逐字节不变。
//!
//! 共用件在 `komo_gateway::service::test_support::harness`（真数据目录、真 `service::start`、
//! 脚本化模型）。这里断言的是**模型收到了什么**，所以读的是 `FakeLlm` 记下的轮输入与请求。

#![allow(unused_imports)]

use std::sync::Arc;

use komo_gateway::service::test_support::harness::{
    FakeLlm, Home, call_round, config_toml, text_round,
};
use komo_kernel::events::{Event, EventPayload, ToolResult};
use komo_kernel::traits::LlmClient;
use komo_kernel::types::resource::{ResourceUri, output_file_path};
use komo_kernel::types::status::RunState;
use komo_kernel::types::turn::RoundInput;

/// 模型被喂回来的工具结果正文（工具结果是从轮输入回去的）：开头是 `[工具名` 的那一条。
fn fed_back(llm: &FakeLlm, tool: &str) -> String {
    let prefix = format!("[{tool} · ");
    llm.inputs
        .lock()
        .expect("轮输入")
        .iter()
        .find_map(|input| match input {
            RoundInput::ToolResults { results, .. } => results
                .iter()
                .find(|result| result.content.starts_with(&prefix))
                .map(|result| result.content.clone()),
            RoundInput::First => None,
        })
        .unwrap_or_else(|| panic!("模型没拿到 {tool} 的结果"))
}

/// 回放窗口里那条工具结果的正文：某个执行段开始装配上下文时，它先从账本+`output.json`
/// 把事实读回来，再投影一次。
fn replayed(llm: &FakeLlm, tool: &str) -> String {
    let prefix = format!("[{tool} · ");
    llm.requests
        .lock()
        .expect("请求")
        .iter()
        .rev()
        .find_map(|request| {
            request
                .messages
                .iter()
                .flat_map(|message| message.tool_results.iter())
                .find(|result| result.content.starts_with(&prefix))
                .map(|result| result.content.clone())
        })
        .unwrap_or_else(|| panic!("回放窗口里没有 {tool} 的结果"))
}

mod durable;
