//! 资源命名空间在 Durable 存储上（`docs/storage-durable-d5.md` W4b）：与 `../main.rs` 同名的 5 个，
//! Gateway 跑在新存储上。
//!
//! 与 Legacy 不同的断言（各测试里另有说明）：
//!
//! - **计划**：Legacy 读事件流里的 `tool.planned.plan`，断言"计划里本地路径没有资源语义、`skill://` 是逻辑
//!   URI"。新存储的合成事件不带计划（`plan: None`），计划只存在工具任务运行期间的 checkpoint 里，任务终结
//!   就随 checkpoint 丢掉，没有落盘的地方可读。这两句断言搬不过来，**删掉**：计划目标的逻辑 URI / 描述由
//!   `komo-runtime/src/tools/read.rs` 的单元测试直接对 `prepare` 的结果断言（`plan.targets[0].describe()`），
//!   这里只留端到端能看到的——模型收到的正文；
//! - **产物位置**：Legacy 的 `tool.result.output_ref` 指向 `tool-output/<run>/<call>/<attempt>/output.json`；
//!   新存储是 `tool-output/{conversation}/{tool_task}/{attempt}/`，`artifact://` 入口的拼法见
//!   `another_sessions_run_is_out_of_reach`；
//! - **停下来问人**：新存储没有 `waiting + intervention`，`interventions().is_empty()` 一句删掉
//!   （读不到别人的输出这件事本身由"错误结果、一个字不漏"证）。

use std::sync::Arc;

use komo_gateway::service::test_support::harness::{
    DEFAULT_ENV, FakeLlm, Home, text_round, write_home,
};
use komo_kernel::types::status::RunState;

use super::{config, fed_back, home_with, read_call, tools};

/// `read` 一个 workspace 里的真文件：正文回来。
#[tokio::test]
async fn a_local_path_reads_as_plain_content_without_resource_semantics() {
    let home = home_with(&config(&tools("\"read\", \"rg\"")));
    let file = home.workspace().join("笔记.txt");
    std::fs::create_dir_all(home.workspace()).expect("工作目录");
    std::fs::write(&file, "本地文件的内容\n").expect("写文件");

    let llm = FakeLlm::new(vec![vec![
        read_call(1, "pc-read", "笔记.txt"),
        text_round(2, "读完了。"),
    ]]);
    let gw = home.start(Arc::clone(&llm)).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "res-local", "读那份笔记").await.run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let results = fed_back(&llm);
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].1, "普通文件读得到：{}", results[0].0);
    assert!(
        results[0].0.contains("本地文件的内容"),
        "正文就是那份文件：{}",
        results[0].0
    );

    gw.stop().await;
}

/// 数据目录里一份真 skill：`read("skill://review/SKILL.md")` 读到正文（计划里的逻辑 URI 见文件头说明）。
#[tokio::test]
async fn a_skill_uri_reads_and_the_plan_names_the_logical_uri() {
    let home = Home::new();
    let skill_dir = home.path().join("skills");
    let skill = skill_dir.join("review").join("SKILL.md");
    std::fs::create_dir_all(skill.parent().expect("skill 目录")).expect("建 skill 目录");
    std::fs::write(
        &skill,
        "---\nname: review\ndescription: 怎么审一个 PR\n---\n# 审查步骤\n先看 diff。\n",
    )
    .expect("写 SKILL.md");
    write_home(
        home.path(),
        &config(&format!(
            "[paths]\nskill_dirs = [\"{}\"]\n\n{}",
            skill_dir.display(),
            tools("\"read\", \"rg\"")
        )),
        DEFAULT_ENV,
    );

    let llm = FakeLlm::new(vec![vec![
        read_call(1, "pc-skill", "skill://review/SKILL.md"),
        text_round(2, "看过了。"),
    ]]);
    let gw = home.start(Arc::clone(&llm)).await;
    let session = gw.open_session().await;
    let run = gw
        .submit(&session, "res-skill", "按审查 skill 看一遍")
        .await
        .run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let results = fed_back(&llm);
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(
        !results[0].1,
        "skill 是只读根，直接读得到：{}",
        results[0].0
    );
    assert!(
        results[0].0.contains("先看 diff"),
        "拿到的是那份 SKILL.md 的正文：{}",
        results[0].0
    );

    gw.stop().await;
}

/// `tool://` 是**虚拟入口**：面内的读到 JSON Schema；面外的在 `prepare` 就拒绝。
#[tokio::test]
async fn tool_uris_read_schema_inside_the_surface_and_are_refused_outside_it() {
    let home = home_with(&config(&tools("\"read\", \"rg\"")));
    let llm = FakeLlm::new(vec![vec![
        read_call(1, "pc-in", "tool://read/schema"),
        read_call(2, "pc-out", "tool://write/schema"),
        text_round(3, "看完了。"),
    ]]);
    let gw = home.start(Arc::clone(&llm)).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "res-tool", "看看工具说明").await.run;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let results = fed_back(&llm);
    assert_eq!(results.len(), 2, "两次读，两条结果：{results:?}");

    // 面内：真的给了 read 的 JSON Schema。
    assert!(!results[0].1, "面内的工具读得到：{}", results[0].0);
    assert!(
        results[0].0.contains("\"properties\"") && results[0].0.contains("\"path\""),
        "面内读到的是 JSON Schema：{}",
        results[0].0
    );

    // 面外：拒绝，而且是一句说清原因的话。
    assert!(results[1].1, "面外的工具是错误结果：{}", results[1].0);
    assert!(
        !results[1].0.contains("\"properties\""),
        "被拒绝的调用什么都没给回来：{}",
        results[1].0
    );
    assert!(
        results[1].0.contains("不在这次能力面里") && results[1].0.contains("write"),
        "那句话要说清是哪个工具、这次能用哪些：{}",
        results[1].0
    );

    gw.stop().await;
}

/// 会话目录 `artifacts/<run>/` 里的历史产物仍可用 `artifact://files/<run>/<名字>` 读回；**重启 Gateway**
/// （同一个数据目录）之后读到的正文与之前一致。
#[tokio::test]
async fn historical_artifacts_stay_readable_after_a_restart() {
    let home = home_with(&config(&tools("\"read\", \"rg\"")));
    let llm = FakeLlm::new(vec![vec![
        read_call(1, "pc-read", "artifact://files/run-old/报告.md"),
        text_round(2, "读到了。"),
    ]]);
    let gw = home.start(Arc::clone(&llm)).await;
    let session = gw.open_session().await;

    let artifacts = home.session_dir(&session).join("artifacts/run-old");
    std::fs::create_dir_all(&artifacts).expect("建产出目录");
    std::fs::write(artifacts.join("报告.md"), "产物正文\n").expect("写历史产物");

    let run_a = gw.submit(&session, "res-art", "读那份产物").await.run;
    let detail = gw.wait_terminal(&run_a).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let results = fed_back(&llm);
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].1, "{}", results[0].0);
    assert!(results[0].0.contains("产物正文"), "{}", results[0].0);
    gw.stop().await;

    let llm_b = FakeLlm::new(vec![vec![
        read_call(1, "pc-read", "artifact://files/run-old/报告.md"),
        text_round(2, "读到了。"),
    ]]);
    let gw_b = home.start(Arc::clone(&llm_b)).await;
    let run_b = gw_b.submit(&session, "res-art-2", "再读一遍").await.run;
    let detail = gw_b.wait_terminal(&run_b).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let results = fed_back(&llm_b);
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].1, "重启之后产物读得回来：{}", results[0].0);
    assert!(
        results[0].0.contains("产物正文"),
        "正文与第一次一致：{}",
        results[0].0
    );

    gw_b.stop().await;
}

/// 会话边界：拿**另一个会话**的工具输出拼出来的 `artifact://…` 读不到。
///
/// 新存储里工具输出在 `tool-output/{conversation}/{tool_task}/{attempt}/`（Session 目录之下），入口拼法
/// 是 `artifact://{conversation}/{tool_task}/{attempt}/result`——Legacy 里第一段是 run。拿甲会话的真
/// 实输出路径去乙会话读：入口指的是乙会话自己的 `tool-output/`，那儿没有这份文件。
#[tokio::test]
async fn another_sessions_run_is_out_of_reach() {
    let home = home_with(&config(&tools("\"read\", \"rg\"")));
    // 会话 A：读一个本地文件，于是有了它自己的 `tool-output/…`。
    let file = home.workspace().join("甲会话的文件.txt");
    std::fs::create_dir_all(home.workspace()).expect("工作目录");
    std::fs::write(&file, "只有甲会话读得到的内容\n").expect("写文件");

    let llm = FakeLlm::new(vec![vec![
        read_call(1, "pc-a", "甲会话的文件.txt"),
        text_round(2, "读完了。"),
    ]]);
    let gw = home.start(Arc::clone(&llm)).await;
    let session_a = gw.open_session().await;
    let run_a = gw.submit(&session_a, "res-a", "读我自己的文件").await.run;
    let detail = gw.wait_terminal(&run_a).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    // 甲会话那条输出的真实位置：从合成事件的 `output_ref` 取（`tool-output/{conversation}/{task}/{attempt}/…`）。
    let result = gw
        .events(&session_a)
        .await
        .into_iter()
        .find_map(|event| match event.payload {
            komo_kernel::events::EventPayload::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("有一条 tool.result");
    let located = home.session_dir(&session_a).join(result.output_ref.path());
    let secret = std::fs::read_to_string(&located).expect("output.json 读得到");
    assert!(secret.contains("只有甲会话读得到的内容"), "{secret}");
    // `tool-output/{conversation}/{task}/{attempt}/output.json` → `artifact://{conversation}/{task}/{attempt}/result`。
    let parts: Vec<&str> = result.output_ref.path().split('/').collect::<Vec<_>>();
    assert_eq!(parts.first(), Some(&"tool-output"), "{parts:?}");
    let uri = format!("artifact://{}/{}/{}/result", parts[1], parts[2], parts[3]);

    // 对照：同一个入口在**甲会话自己**里读得到（否则下面"读不到"可能只是入口写坏了——新存储的入口是
    // 十进制的 `{conversation}/{task}/{attempt}`，`ResourceUri::parse` 曾经只认 UUID，W4b 修了）。
    gw.stop().await;
    let llm_a = FakeLlm::new(vec![vec![
        read_call(1, "pc-own", &uri),
        text_round(2, "读到了。"),
    ]]);
    let gw_a = home.start(Arc::clone(&llm_a)).await;
    let run_own = gw_a
        .submit(&session_a, "res-a2", "再读一遍我自己的输出")
        .await
        .run;
    let detail = gw_a.wait_terminal(&run_own).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let own = fed_back(&llm_a);
    assert_eq!(own.len(), 1, "{own:?}");
    assert!(!own[0].1, "自己的输出读得到：{}", own[0].0);
    assert!(
        own[0].0.contains("只有甲会话读得到的内容"),
        "正文是那份输出：{}",
        own[0].0
    );

    // 停机再起（同一个数据目录），开一个**新会话**，拿甲会话的输出入口去读。
    gw_a.stop().await;
    let llm_b = FakeLlm::new(vec![vec![
        read_call(1, "pc-b", &uri),
        text_round(2, "读不到。"),
    ]]);
    let gw_b = home.start(Arc::clone(&llm_b)).await;
    let session_b = gw_b.open_session().await;
    assert_ne!(session_a, session_b);
    let run_b = gw_b.submit(&session_b, "res-b", "读别人的输出").await.run;
    let detail = gw_b.wait_terminal(&run_b).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let results = fed_back(&llm_b);
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(results[0].1, "读不到就是错误结果：{}", results[0].0);
    assert!(
        !results[0].0.contains("只有甲会话读得到的内容"),
        "另一个会话的输出一个字都不该漏：{}",
        results[0].0
    );

    gw_b.stop().await;
}
