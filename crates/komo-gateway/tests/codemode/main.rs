//! codemode（`docs/codemode.md` §10）：真 Gateway，worker 是进程内的（测试进程自己不是 `komo`；
//! 真 worker 进程的验收在 `crates/komo/tests/codemode_worker.rs`）。

use komo_gateway::service::test_support::harness::FakeLlm;
use komo_kernel::types::turn::RoundInput;

fn tool_results(llm: &FakeLlm) -> Vec<String> {
    llm.inputs
        .lock()
        .unwrap()
        .iter()
        .filter_map(|input| match input {
            RoundInput::ToolResults { results, .. } => Some(
                results
                    .iter()
                    .map(|result| result.content.clone())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect()
}

mod durable;
