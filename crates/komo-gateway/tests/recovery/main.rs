//! D5 W4a：重启恢复的端到端验收，跑在 Durable 存储上（`docs/storage-durable-d5.md` §4 W4a、决定 16）。
//!
//! 旧 `tests/recovery` 是 §14「恢复故障注入验收」，逐行钉死旧账本（`state.db` + JSONL）在 §8.5 各步骤之间被
//! 打断的行为，靠故障账本装饰器注入。新存储的恢复由引擎开库时的 reconciliation 做，逐步的故障注入在
//! runtime 的测试里覆盖（`harness/tests/{recovery,tools}.rs`、`durable/tests/recovery.rs`）；gateway 这一层只
//! 保留 5 个**端到端重启**测试，每个起一台真 Gateway（`service::start`）、停机、同一个数据目录再起：
//!
//! | 测试 | 钉住什么 |
//! |---|---|
//! | `the_first_end_to_end_acceptance` | §14「首个端到端验收」：重启后不开任何客户端自动接续原 run，再 resume / 读文件 |
//! | `a_chat_run_in_flight_across_a_restart_still_gets_its_reply_delivered_once` | 重启后看客补挂：聊天来源的 run 收到回复，只投一次 |
//! | `a_background_task_in_flight_across_a_restart_still_reports_back` | 后台任务跨重启，结果仍交回 home |
//! | `a_killed_bash_is_handed_to_the_model_as_interrupted` | 在飞的 `bash` 重启后不重跑，`uncertain` 交给模型 |
//! | `a_missing_blob_faults_the_tool_task_and_lists_it` | blob 引用读不到 → 工具任务 `faulted`，列进 intervention、投 NeedsAttention、可 `abandon` |
//!
//! 旧 `tests/recovery/{rows,verdicts,reconcile}.rs` 保持只跑 Legacy（D7 删）。"停在 `waiting + intervention`"
//! 在新存储里不存在，原来靠 `unsure` 工具停住的步骤换成 `gate` 工具（停在 running 上，重启后按
//! checkpoint 续跑：没有结果的那次调用落成 `uncertain`，不重跑）。

mod bash;
mod blob;
mod chat;
mod e2e;
mod task;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use komo_gateway::channels::ChannelSender;
use komo_gateway::service::test_support::harness::{
    FakeLlm, FixedFactory, GatewayBuilder, MemSender, TestGateway, telegram_config,
};
use komo_kernel::traits::Tool;
use komo_kernel::types::chat::ChannelPlatform;

/// 起一台 Durable Gateway。`home = Some(..)` 就是"重启"（同一个数据目录）。
pub async fn start(
    home: Option<&Path>,
    llm: Arc<FakeLlm>,
    tools: Vec<Arc<dyn Tool>>,
    sender: &Arc<MemSender>,
) -> TestGateway {
    start_with(&telegram_config("111"), home, llm, tools, sender).await
}

/// 同上，Agent 多挂 `bash`（默认工具表里没有它）。
pub async fn start_bash(
    home: Option<&Path>,
    llm: Arc<FakeLlm>,
    sender: &Arc<MemSender>,
) -> TestGateway {
    let config = telegram_config("111").replace(
        "[agents.assistant]\n",
        "[agents.assistant]\ntools = [\"bash\"]\n",
    );
    start_with(&config, home, llm, vec![], sender).await
}

async fn start_with(
    config: &str,
    home: Option<&Path>,
    llm: Arc<FakeLlm>,
    tools: Vec<Arc<dyn Tool>>,
    sender: &Arc<MemSender>,
) -> TestGateway {
    let mut builder = GatewayBuilder::new(config)
        .fake_llm(llm)
        .factory(FixedFactory::sender_only(
            Arc::clone(sender) as Arc<dyn ChannelSender>
        ));
    for tool in tools {
        builder = builder.tool(tool);
    }
    if let Some(home) = home {
        builder = builder.at(home);
    }
    builder.start().await
}

pub fn sender() -> Arc<MemSender> {
    MemSender::new(ChannelPlatform::Telegram)
}

/// 轮询到条件成立（最多 20 秒）。
pub async fn until<F, Fut>(what: &str, mut done: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if done().await {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "等了 20 秒：{what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 一个会话全部 entry 的 JSON（断言"有没有写过 / 读到过什么"用）。
pub async fn entries_json(gateway: &TestGateway, session: &str) -> String {
    let core = komo_gateway::service::test_support::driver_stream::core(gateway);
    let tree = core.read_tree(session).await.expect("读得出树");
    serde_json::to_string(
        &tree
            .branches
            .iter()
            .flat_map(|branch| branch.entries.iter())
            .collect::<Vec<_>>(),
    )
    .expect("可序列化")
}
