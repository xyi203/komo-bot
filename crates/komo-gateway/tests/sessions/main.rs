//! `POST /v1/sessions` 的 `workdir`：**界面读的那一份与执行用的那一份必须是同一份**
//! （§13.1、§8.10）。
//!
//! 这条路径原先有两份事实——界面从进程内的影子表读，执行从 `sessions.workdir` 那一行读，
//! 于是"你看见的目录"和"真正跑的目录"可以不一样，而且重启就丢。这里断言的是收口之后
//! 的样子：创建时落库、界面读那一行、执行也用那一行；换一个 `Db` 句柄（不是同一份内存
//! 缓存）之后仍然是它。
//!
//! 共用件在 `komo_gateway::service::test_support::harness`（真数据目录、真 `service::start`、
//! 脚本化模型）。

use std::sync::Arc;

use komo_gateway::service::test_support::harness::{
    FakeLlm, Home, call_round, config_toml, text_round,
};
use komo_kernel::traits::LlmClient;
use komo_kernel::types::status::RunState;
use komo_kernel::types::turn::RoundInput;

/// 只在该目录下才存在的那一行：相对路径读得回来，才说明 cwd 真的是它。
const ONLY_IN_WORKDIR: &str = "只有这个工作目录里有的一行 42";

/// 模型被喂回来的工具结果正文（工具结果从轮输入回去）：开头是 `[工具名` 的那一条。
fn fed_back(llm: &FakeLlm, tool: &str) -> String {
    let prefix = format!("[{tool} · ");
    llm.inputs
        .lock()
        .expect("轮输入")
        .iter()
        .find_map(|input| match input {
            RoundInput::ToolResults { results } => results
                .iter()
                .find(|result| result.content.starts_with(&prefix))
                .map(|result| result.content.clone()),
            RoundInput::First => None,
        })
        .unwrap_or_else(|| panic!("模型没拿到 {tool} 的结果"))
}

#[tokio::test]
async fn a_session_workdir_is_one_fact_for_the_interface_and_the_execution() {
    let home = Home::with_config(&config_toml(""));
    // 一个**不在** Gateway 数据目录里的工作目录：执行真的要在这里跑。
    let workdir = tempfile::tempdir().expect("工作目录");
    let workdir_str = workdir.path().display().to_string();
    std::fs::write(
        workdir.path().join("只有这里.txt"),
        format!("{ONLY_IN_WORKDIR}\n"),
    )
    .expect("写文件");

    let llm = FakeLlm::new(vec![vec![
        // 相对路径：cwd 不是那个目录就什么都读不到。
        call_round(
            1,
            "pc-read",
            "read",
            serde_json::json!({ "path": "只有这里.txt" }),
        ),
        text_round(2, "读完了。"),
    ]]);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    // 创建会话时带上 workdir：摘要立刻就该报它。
    let (code, body) = gateway
        .post(
            "/v1/sessions",
            serde_json::json!({ "workdir": workdir_str }),
        )
        .await;
    assert_eq!(code, 200, "{body}");
    let summary: komo_kernel::protocol::http::SessionSummary =
        serde_json::from_str(&body).expect("会话");
    let session = summary.session.clone();
    assert_eq!(
        summary.workdir.as_deref(),
        Some(workdir_str.as_str()),
        "创建时给的 workdir 要原样出现在摘要里"
    );

    // 详情读的是同一条。
    let (code, body) = gateway.get(&format!("/v1/sessions/{session}")).await;
    assert_eq!(code, 200, "{body}");
    let detail: komo_kernel::protocol::http::SessionDetail =
        serde_json::from_str(&body).expect("详情");
    assert_eq!(
        detail.summary.workdir.as_deref(),
        Some(workdir_str.as_str()),
        "详情与摘要不能各有各的 workdir"
    );

    // 执行用的也是它：相对路径只有从那个目录解析得到。
    let run = gateway
        .submit(&session, "sessions-1", "读一下工作目录里的那份文件")
        .await
        .run;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let read_back = fed_back(&llm, "read");
    assert!(
        read_back.contains(ONLY_IN_WORKDIR),
        "执行没有在创建时给的那个目录里跑：\n{read_back}"
    );

    gateway.stop().await;

    // 换一个 `Db` 句柄（相当于新进程）：仍然是它，不是某一份内存缓存。
    let db = home.open_db().await;
    let record = komo_store::repos::session::get(&db, &session)
        .await
        .expect("读得出")
        .expect("有这个会话");
    assert_eq!(
        record.workdir.as_deref(),
        Some(workdir_str.as_str()),
        "workdir 必须落在 sessions.workdir 那一行上"
    );
}
