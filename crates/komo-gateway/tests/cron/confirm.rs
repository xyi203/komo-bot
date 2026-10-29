//! 对外动作确认（`docs/confirm.md`）在真 Gateway 上：Cron 触发的 Run 调一个声明为对外的 MCP 工具
//! （`tests/mcp/server.py` 的 `bump`，每执行一次往计数文件写一行），先列进 intervention、投一张卡到
//! home chat，答了才执行。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use komo_kernel::protocol::http::{InterventionKind, InterventionVerdict};
use komo_kernel::types::chat::Outbound;
use komo_kernel::types::turn::RoundInput;
use komo_runtime::durable::harness::{ConfirmAnswer, ConfirmVerdict, confirm_address};

use crate::harness::*;

fn daily() -> serde_json::Value {
    serde_json::json!({
        "name": "bump-daily",
        "schedule": "0 9 * * *",
        "timezone": "Asia/Shanghai",
        "prompt": "加一",
    })
}

/// 默认的 home（Telegram 配了 home chat）加一台 `demo` 服务器，`bump` 声明为对外。
fn home_with_outward() -> (Home, PathBuf) {
    let home = Home::new();
    let server = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mcp/server.py");
    let counter = home.path().join("bump.count");
    let config = format!(
        r#"{}
[mcp.servers.demo]
command = "python3"
args = ["{}", "{}"]
exposure = "direct"
outward = ["bump"]
"#,
        home_config(),
        server.display(),
        counter.display(),
    );
    std::fs::write(home.path().join("config.toml"), config).unwrap();
    (home, counter)
}

fn bump_then(text: &str) -> Arc<FakeLlm> {
    FakeLlm::new(vec![vec![
        call_round(1, "pc-1", "mcp__demo__bump", serde_json::json!({})),
        text_round(2, text),
    ]])
}

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

#[tokio::test]
async fn a_cron_run_asks_before_an_outward_mcp_call_and_runs_it_once_approved() {
    let (home, counter) = home_with_outward();
    let gw = home.start(bump_then("加好了")).await;
    let job = gw.add_job(daily()).await;
    gw.make_due(&job.id).await;
    let run = gw.tick().await.fired[0].run.clone();

    let pending = gw.wait_intervention(&run).await;
    assert_eq!(pending.kind, InterventionKind::Confirm);
    assert_eq!(
        pending.verdicts,
        vec![InterventionVerdict::Approve, InterventionVerdict::Reject]
    );
    assert_eq!(lines(&counter), 0, "答之前不执行");

    let sender = Arc::clone(&gw.home);
    let handle = pending.handle.clone();
    eventually("确认卡投到 home chat", move || {
        sender.sent().iter().any(|message| {
            matches!(&message.outbound, Outbound::NeedsAttention { reason, .. }
                if reason.contains("mcp__demo__bump")
                    && reason.contains("定时任务「bump-daily」")
                    && reason.contains(&format!("/answer {handle} approve")))
        })
    })
    .await;

    let (code, body) = gw
        .post_json(
            &format!("/v1/interventions/{}/answer", pending.handle),
            serde_json::json!({ "verdict": "abandon" }),
        )
        .await;
    assert_eq!(code, 422, "{body}");
    gw.answer(&pending.handle, "approve").await;
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;
    assert_eq!(lines(&counter), 1, "批准后只执行一次");
    assert!(gw.interventions().await.is_empty());

    // 谁、什么时候答的留在 `komo.confirm` 里。
    let core = gw.state().core().unwrap().clone();
    let task: u64 = pending.handle.parse().unwrap();
    let record = core
        .storage
        .task(komo_kernel::durable::TaskId::new(task))
        .await
        .unwrap()
        .unwrap();
    let answer: ConfirmAnswer = serde_json::from_value(
        core.storage
            .find_document(&confirm_address(record.conversation, record.id))
            .await
            .unwrap()
            .unwrap()
            .value,
    )
    .unwrap();
    assert_eq!(
        (answer.verdict, answer.by.as_str()),
        (ConfirmVerdict::Approve, "cli")
    );
    gw.stop().await;
}

#[tokio::test]
async fn a_rejected_card_never_runs_the_call_and_the_model_hears_why() {
    let (home, counter) = home_with_outward();
    let llm = bump_then("那就不加了");
    let gw = home.start(Arc::clone(&llm)).await;
    let job = gw.add_job(daily()).await;
    gw.make_due(&job.id).await;
    let run = gw.tick().await.fired[0].run.clone();

    let pending = gw.wait_intervention(&run).await;
    gw.answer(&pending.handle, "reject").await;
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;
    assert_eq!(lines(&counter), 0);
    let results = tool_results(&llm);
    assert!(
        results
            .iter()
            .any(|text| text.contains("操作者拒绝了这次 mcp__demo__bump 调用")),
        "{results:?}"
    );
    gw.stop().await;
}

#[tokio::test]
async fn a_user_run_calls_the_same_outward_tool_without_a_card() {
    let (home, counter) = home_with_outward();
    let gw = home.start(bump_then("加好了")).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "bump-1", "加一").await.run;
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;
    assert_eq!(lines(&counter), 1);
    assert!(gw.interventions().await.is_empty());
    gw.stop().await;
}
