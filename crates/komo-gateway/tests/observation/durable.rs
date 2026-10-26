//! §8.3 的「工具结果正文」与 `docs/komo_observation.md` 的 M1–M3，Durable 存储版（`docs/storage-durable-d5.md`
//! W4b）。
//!
//! 新存储里**结果的模型视图只算一次、存进 entry**（`tool.rs`：收尾时 `project_views` + `select`，结果 entry 的
//! `model` 就是 full 视图，`data.decayed` 放短视图），之后任何一次请求（包括重启后）读的都是这份存下来的
//! 字节。所以 Legacy "活着渲染 / 回放渲染要逐字节相同"这一类测试，论点从"两条路径算出同一份"变成
//! "存下来的那一份原样回放"，下面逐个说明。
//!
//! 对应关系与改写（理由）：
//!
//! | Legacy | Durable |
//! |---|---|
//! | `a_big_tool_output_keeps_both_ends_and_the_uri_it_points_at_really_reads` | 同名；输出落点读合成事件的 `output_ref`，其余同 |
//! | `the_replayed_window_renders_the_same_bytes_as_the_live_round` | 同名；"同一个 Run 的第二段"换成"重启后同一会话的下一条 Run"（停在介入上的 `unsure` 不存在） |
//! | `a_fused_edit_runs_once_and_replays_the_same_bytes` | 同名；同上 |
//! | `a_refused_call_renders_the_same_bytes_live_and_replayed` | 同名；同上 |
//! | `a_hot_reload_does_not_change_how_a_running_run_projects` | `a_hot_reload_changes_the_budget_for_results_finished_after_it_and_leaves_stored_ones`：投影设置不再冻在受理快照里，每个工具调用收尾时按当前配置算，已存的不变 |
//! | `the_decayed_view_is_the_same_live_and_replayed` | 同名；`revised` 不存在，短视图是下一次请求里那条消息换了视图 |
//! | `a_short_task_never_rewrites_a_big_result` | 同名 |
//! | `resuming_after_the_cache_went_cold_decays_at_the_segment_start` | **不迁**：靠 `install_cache_clock_skew` 拨快旧账本的"现在"；新存储里缓存冷不冷是 runtime 的纯判断，`harness/tests/compaction.rs::a_cold_cache_decays_every_eligible_result_without_window_pressure` 钉着 |
//! | `a_compacted_run_replays_the_same_summary_and_tail_across_restarts` | **不迁**：靠 `turn_ledger.record_compaction` 往旧账本补一条压缩事件；压缩 + 重启回放逐字节不变由 `tests/compaction` 的 `window_protection_…`（重启后前缀逐字节相同）与 runtime 的压缩测试覆盖 |

use std::sync::Arc;

use komo_gateway::service::test_support::driver_stream::RecordingStream;
use komo_gateway::service::test_support::harness::{
    FakeLlm, GatewayBuilder, Home, TestGateway, call_round, config_toml, text_round,
};
use komo_kernel::events::{ContextCompacted, Event, EventPayload, ToolResult};
use komo_kernel::types::resource::{ResourceUri, output_file_path};
use komo_kernel::types::status::RunState;
use komo_runtime::llm::ModelStream;

use super::{fed_back, replayed};

/// 这个 Session 里所有 `tool.result`，按顺序。
fn tool_results(events: &[Event]) -> Vec<&ToolResult> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolResult(body) => Some(body),
            _ => None,
        })
        .collect()
}

fn big_file_home(config: &str) -> Home {
    let home = Home::with_config(config);
    let lines: String = (1..=5000).map(|n| format!("行 {n}\n")).collect();
    std::fs::create_dir_all(home.workspace()).expect("工作目录");
    std::fs::write(home.workspace().join("大文件.txt"), &lines).expect("写文件");
    home
}

fn home_with_a_big_file() -> Home {
    // 上下文预算调到 2 KiB：这条测试要看的就是"超了预算会怎样"（§6）。
    big_file_home(&config_toml("[execution]\nmodel_result_bytes = 2048\n"))
}

#[tokio::test]
async fn a_big_tool_output_is_previewed_visibly_and_the_uri_it_points_at_really_reads() {
    let home = home_with_a_big_file();
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-read",
            "read",
            serde_json::json!({ "path": "大文件.txt" }),
        ),
        text_round(2, "读完了。"),
    ]]);
    let gateway = home.start(Arc::clone(&llm)).await;
    let session = gateway.open_session().await;
    let run = gateway
        .submit(&session, "obs-1", "把那份大文件读一遍")
        .await
        .run;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let live = fed_back(&llm, "read");
    // 抬头、开头预览、裁了多少——不是悄悄"砍掉一半"。
    assert!(live.starts_with("[read · 完成 · "), "{live}");
    assert!(
        live.contains("超过单条结果上限"),
        "超了预算要说裁了多少：\n{live}"
    );
    assert!(live.contains("行 1"), "头部要留着：\n{live}");
    // 后面那一段（行 300 左右）不在预览里。
    assert!(
        !live.contains("行 300"),
        "被省略的部分不该声称给了：\n{live}"
    );

    // 完整输出的引用是一条**资源入口**，它指的正是这次落盘的那一份 `output.json`——拼法只有一处
    // （`output_file_path`），两边共用。
    let events = gateway.events(&session).await;
    let result = tool_results(&events).pop().expect("有结果");
    let output = home.session_dir(&session).join(result.output_ref.path());
    let readable = std::fs::canonicalize(&output).expect("output.json 真的在");
    let entry = live
        .lines()
        .find_map(|line| line.strip_prefix("完整输出："))
        .expect("正文里要给出去哪儿读")
        .split('（')
        .next()
        .expect("入口")
        .to_string();
    let uri = ResourceUri::parse(&entry).expect("印出来的入口是合法的资源 URI");
    assert!(
        matches!(uri, ResourceUri::Output { .. }),
        "印出来的该是工具输出那条入口：{entry}"
    );
    let pointed = output_file_path(&home.session_dir(&session), &uri).expect("那条入口有路径");
    assert_eq!(
        std::fs::canonicalize(&pointed).expect("入口指向的文件真的在"),
        readable,
        "印出来的入口要正好指向落盘的那一份"
    );

    // ── 再用**同一条入口**发起一次 `read`：模型原样交回来就真读得进去（Session 自己的输出是只读根）。
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-again", "read", serde_json::json!({ "path": entry })),
        text_round(2, "打开了。"),
    ]]);
    gateway.stop().await;
    // 首次请求刻意用 2 KiB 测截断；重新读单行 JSON 时恢复默认 30 KiB。
    // pi 的 head 视图不切开长行，因此 2 KiB 无法展示这一整行。
    std::fs::write(home.path().join("config.toml"), config_toml("")).expect("恢复默认模型输出预算");
    let gateway = home.start(Arc::clone(&llm)).await;
    let run = gateway
        .submit(&session, "obs-2", "把完整输出读出来")
        .await
        .run;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    // 省略不等于丢失：模型没看见的那一行，就在那条入口指向的文件里。
    let on_disk = std::fs::read_to_string(&readable).expect("读 output.json");
    assert!(on_disk.contains("行 300"), "被省略的那一段就在完整输出里");

    // 而模型确实能从那条入口读回来。
    let read_back = fed_back(&llm, "read");
    assert!(
        read_back.contains("\"v\":1"),
        "读到的是那份 output.json：\n{read_back}"
    );
}

/// 存下来的结果视图，重启之后在同一会话的下一条 Run 的请求里**逐字节相同**。
///
/// Legacy 让同一个 Run 停在介入上、答复后起第二段来回放；新存储里"第二段"就是下一次请求，而它读的是结果
/// entry 里存着的那份视图，所以这里用最直接的版本：重启 Gateway、同一会话再提交一条，看新请求里那条工具
/// 结果与刚跑完那次交回模型的一样。
#[tokio::test]
async fn the_replayed_window_renders_the_same_bytes_as_the_live_round() {
    let home = home_with_a_big_file();
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-read",
            "read",
            serde_json::json!({ "path": "大文件.txt" }),
        ),
        text_round(2, "都做完了。"),
    ]]);
    let gateway = home.start(Arc::clone(&llm)).await;
    let session = gateway.open_session().await;
    let run = gateway.submit(&session, "obs-3", "读一遍大文件").await.run;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let live = fed_back(&llm, "read");
    gateway.stop().await;

    let llm2 = FakeLlm::finisher("好。");
    let gateway = home.start(Arc::clone(&llm2)).await;
    let run = gateway.submit(&session, "obs-3b", "再说一句").await.run;
    gateway.wait_terminal(&run).await;
    assert_eq!(
        replayed(&llm2, "read"),
        live,
        "同一份事实在回放时渲染出的字节必须与刚跑完那次一样"
    );
}

/// `edit` 带 `then_run`（Action Fusion）：改动与命令各跑一次，结果是**一条**；重启后下一条 Run 的请求里，
/// 那条结果的字节与刚交回模型的一样。
#[tokio::test]
async fn a_fused_edit_runs_once_and_replays_the_same_bytes() {
    let home = Home::with_config(&config_toml(""));
    std::fs::create_dir_all(home.workspace()).expect("工作目录");
    let file = home.workspace().join("app.txt");
    std::fs::write(&file, "version = 1\n").expect("写文件");
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "pc-edit",
            "edit",
            serde_json::json!({
                "path": "app.txt",
                "match_text": "version = 1",
                "replace_text": "version = 2",
                "then_run": { "command": "echo ran >> ran.txt; cat app.txt" },
            }),
        ),
        text_round(2, "改完也跑过了。"),
    ]]);
    let gateway = home.start(Arc::clone(&llm)).await;
    let session = gateway.open_session().await;
    let run = gateway
        .submit(&session, "obs-fused", "改版本号然后跑一下")
        .await
        .run;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    assert_eq!(std::fs::read_to_string(&file).unwrap(), "version = 2\n");
    let ran = std::fs::read_to_string(home.workspace().join("ran.txt")).expect("命令跑过");
    assert_eq!(ran.lines().count(), 1, "命令只跑了一次：{ran:?}");

    let live = fed_back(&llm, "edit");
    assert!(live.starts_with("[edit · 完成 · "), "{live}");
    assert!(live.contains("[then_run:succeeded]"), "{live}");
    assert!(
        live.contains("version = 2"),
        "命令的输出在同一条结果里：{live}"
    );
    let events = gateway.events(&session).await;
    let results = tool_results(&events);
    assert_eq!(results.len(), 1, "组合调用的结果是一条");
    assert!(
        results[0]
            .preview
            .as_deref()
            .is_some_and(|p| p.contains("then_run")),
        "{:?}",
        results[0].preview
    );
    assert!(
        results[0].stdout.is_some(),
        "命令输出落在这次尝试的 stdout 里"
    );
    gateway.stop().await;

    let llm2 = FakeLlm::finisher("好。");
    let gateway = home.start(Arc::clone(&llm2)).await;
    let run = gateway
        .submit(&session, "obs-fused-2", "再说一句")
        .await
        .run;
    gateway.wait_terminal(&run).await;
    assert_eq!(
        replayed(&llm2, "edit"),
        live,
        "组合调用的结果回放时渲染出的字节必须与刚交回模型的那次一样"
    );
    let ran = std::fs::read_to_string(home.workspace().join("ran.txt")).expect("命令跑过");
    assert_eq!(ran.lines().count(), 1, "之后不会再跑一次：{ran:?}");
}

/// 没跑起来的调用（工具名不认识）同样只有一份投影：它的结论落了盘，重启后下一条 Run 的请求里渲染出的字节
/// 与刚交回模型的那次一样。
#[tokio::test]
async fn a_refused_call_renders_the_same_bytes_live_and_replayed() {
    let home = Home::with_config(&config_toml(""));
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-nope", "nope", serde_json::json!({})),
        text_round(2, "没有这个工具，算了。"),
    ]]);
    let gateway = home.start(Arc::clone(&llm)).await;
    let session = gateway.open_session().await;
    let run = gateway
        .submit(&session, "obs-4", "调一个不存在的工具")
        .await
        .run;
    let detail = gateway.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let live = fed_back(&llm, "nope");
    assert!(live.starts_with("[nope · 失败 · "), "{live}");
    assert!(live.contains("没有 nope"), "{live}");
    gateway.stop().await;

    let llm2 = FakeLlm::finisher("好。");
    let gateway = home.start(Arc::clone(&llm2)).await;
    let run = gateway.submit(&session, "obs-4b", "再说一句").await.run;
    gateway.wait_terminal(&run).await;
    assert_eq!(
        replayed(&llm2, "nope"),
        live,
        "没跑起来的调用，回放时渲染出的字节也必须与刚交回模型的那次一样"
    );
}

/// 起一台 Durable Gateway，带逐次请求的记录与 `gate` 工具。
async fn recorded(
    home: &Home,
    config: &str,
    llm: &Arc<FakeLlm>,
    gate: Option<&Arc<komo_gateway::service::test_support::Gate>>,
) -> (TestGateway, Arc<RecordingStream>) {
    let recording = RecordingStream::new(Arc::clone(llm) as Arc<dyn ModelStream>);
    let mut builder = GatewayBuilder::new(config)
        .fake_llm(Arc::clone(llm))
        .stream(Arc::clone(&recording) as Arc<dyn ModelStream>)
        .at(home.path());
    if let Some(gate) = gate {
        builder = builder.tool(gate.tool());
    }
    (builder.start().await, recording)
}

/// 工具结果的投影预算在**每个调用收尾时**按当前配置算（`env.projection`），结果存下来之后不再变：Run 在飞
/// 时把预算改小并热重载，之后收尾的调用按新预算，已经存下的那条在后面的请求里字节不变。
///
/// 与 Legacy 相反（`a_hot_reload_does_not_change_how_a_running_run_projects`：投影设置冻在受理快照里，
/// 热重载只影响之后受理的 Run）——新存储没有受理快照，"names resolved at each use"。
#[tokio::test]
async fn a_hot_reload_changes_the_budget_for_results_finished_after_it_and_leaves_stored_ones() {
    use komo_gateway::service::test_support::Gate;
    let config = config_toml("[execution]\nmodel_result_bytes = 2048\n");
    let home = big_file_home(&config);
    let gate = Gate::new();
    let read = || serde_json::json!({ "path": "大文件.txt" });
    let llm = FakeLlm::new(vec![vec![
        call_round(1, "pc-read-1", "read", read()),
        call_round(2, "pc-gate", "gate", serde_json::json!({})),
        call_round(3, "pc-read-2", "read", read()),
        text_round(4, "都做完了。"),
    ]]);
    let (mut gateway, recording) = recorded(&home, &config, &llm, Some(&gate)).await;
    let session = komo_gateway::service::test_support::driver_stream::open_session(&gateway).await;
    let accepted = komo_gateway::service::test_support::driver_stream::submit(
        &gateway,
        &session,
        "obs-5",
        "读一遍、等门、再读一遍",
    )
    .await;
    gate.wait_entered(1).await;

    // Run 停着的时候把预算改小、衰减关掉，然后热重载。
    gateway.write_config(&config_toml(
        "[execution]\nmodel_result_bytes = 512\ndecay_threshold_bytes = 0\n",
    ));
    gateway.reload().await;
    gate.release();
    gateway.wait_run(&accepted.run).await;

    let seen = recording.seen();
    let last = seen.last().expect("有请求");
    let content = |id: &str| {
        last.tool_results
            .iter()
            .find(|(call, _)| call == id)
            .map(|(_, text)| text.clone())
            .unwrap_or_else(|| panic!("最后一次请求里没有 {id} 的结果：{:?}", last.tool_results))
    };
    let first = content("pc-read-1");
    let second = content("pc-read-2");
    assert!(
        first.len() > 1500,
        "热重载之前收尾的按 2 KiB：{}",
        first.len()
    );
    assert!(
        second.len() < first.len() && second.len() < 1500,
        "热重载之后收尾的按新预算：{} 对 {}",
        second.len(),
        first.len()
    );
    // 已经存下的那条在每一次后续请求里字节不变。
    let earlier = seen
        .iter()
        .filter(|request| request.tool_results.iter().any(|(id, _)| id == "pc-read-1"))
        .map(|request| {
            request
                .tool_results
                .iter()
                .find(|(id, _)| id == "pc-read-1")
                .map(|(_, text)| text.clone())
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(
        earlier.len() >= 2 && earlier.iter().all(|text| text == &first),
        "{earlier:?}"
    );
    gateway.stop().await;
}

/// 读一个大文件 → 两轮小调用 → 收尾：第四次请求时大结果已完整给过两次。
fn decay_script()
-> Vec<Vec<Result<komo_kernel::types::turn::Round, komo_kernel::types::turn::LlmError>>> {
    vec![vec![
        call_round(
            1,
            "pc-big",
            "read",
            serde_json::json!({ "path": "大文件.txt" }),
        ),
        call_round(2, "pc-s1", "read", serde_json::json!({ "path": "小.txt" })),
        call_round(3, "pc-s2", "read", serde_json::json!({ "path": "小.txt" })),
        text_round(4, "都做完了。"),
    ]]
}

fn home_with_big_and_small(config: &str) -> Home {
    let home = big_file_home(config);
    std::fs::write(home.workspace().join("小.txt"), "一行\n").expect("写文件");
    home
}

/// 这个 Session 里记下的 `decayed` 决定。
fn decayed_decisions(events: &[Event]) -> Vec<&ContextCompacted> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ContextCompacted(body @ ContextCompacted::Decayed { .. }) => Some(body),
            _ => None,
        })
        .collect()
}

const TASK: &str = "读一遍大文件，再看两眼小文件，最后收尾";

/// §8.3 的衰减（缓存优先）：窗口压力让一批结果换成短视图，决定落成一条 `komo.decay`（合成事件
/// `Decayed`）；下一次请求里那条大结果换成短视图，重启后同一会话的下一条 Run 读到的仍是同一份短视图。
///
/// 窗口小到上下文一直贴着线，压缩关着：读一个大文件 → 两轮小调用（大结果已完整给过两次，有资格了）→
/// 下一次请求前换短视图。
#[tokio::test]
async fn the_decayed_view_is_the_same_live_and_replayed() {
    let config = config_toml("").replace(
        "model = \"gpt-test\"\n",
        "model = \"gpt-test\"\ncontext_window = 2000\n",
    );
    let home = home_with_big_and_small(&config);
    let llm = FakeLlm::new(decay_script());
    let (mut gateway, recording) = recorded(&home, &config, &llm, None).await;
    let session = komo_gateway::service::test_support::driver_stream::open_session(&gateway).await;
    let accepted = komo_gateway::service::test_support::driver_stream::submit(
        &gateway, &session, "obs-6", TASK,
    )
    .await;
    gateway.wait_run(&accepted.run).await;

    let seen = recording.seen();
    assert_eq!(seen.len(), 4, "四次请求");
    let big = |index: usize| -> String {
        seen[index]
            .tool_results
            .iter()
            .find(|(id, _)| id == "pc-big")
            .map(|(_, text)| text.clone())
            .unwrap_or_else(|| panic!("第 {index} 次请求里没有 pc-big"))
    };
    let full = big(1);
    assert!(full.len() > 4096, "完整视图要超过默认阈值：{}", full.len());
    assert_eq!(big(2), full, "有资格之前不换");
    let live = big(3);
    assert!(live.contains("这份结果前文已完整给过"), "{live}");
    assert!(
        live.contains("行 1\n") && live.contains("…（中间省略 "),
        "{live}"
    );
    assert!(live.contains("完整输出：artifact://"), "{live}");
    assert!(live.len() + 1024 <= full.len());

    let events = gateway.events(&session).await;
    let [
        ContextCompacted::Decayed {
            calls, decision, ..
        },
    ] = decayed_decisions(&events).as_slice()
    else {
        panic!("日志里恰有一条 decayed：{:?}", decayed_decisions(&events));
    };
    assert_eq!(calls.len(), 1);
    assert_eq!(
        decision.reason,
        komo_kernel::compaction::CompactionReason::WindowProtection
    );
    gateway.stop().await;

    // 重启后同一会话的下一条 Run：那条大结果仍是同一份短视图。
    let llm2 = FakeLlm::finisher("好。");
    let (mut gateway, recording2) = recorded(&home, &config, &llm2, None).await;
    let accepted = komo_gateway::service::test_support::driver_stream::submit(
        &gateway,
        &session,
        "obs-6b",
        "再说一句",
    )
    .await;
    gateway.wait_run(&accepted.run).await;
    let first = recording2.seen().remove(0);
    let replayed = first
        .tool_results
        .iter()
        .find(|(id, _)| id == "pc-big")
        .map(|(_, text)| text.clone())
        .expect("回放里有那条 read 结果");
    assert_eq!(
        replayed, live,
        "活着时换上的短视图与重启后回放给出的必须逐字节相同"
    );
    gateway.stop().await;
}

/// 本次改动要修的回归：短任务里大结果从不被改写。多轮请求都带着大结果，没有窗口压力、缓存是热的——前缀
/// 只追加：日志里没有 `decayed`，大结果每次都是完整正文；重启后回放给出的还是同一份完整正文。
#[tokio::test]
async fn a_short_task_never_rewrites_a_big_result() {
    let config = config_toml("");
    let home = home_with_big_and_small(&config);
    let llm = FakeLlm::new(decay_script());
    let (mut gateway, recording) = recorded(&home, &config, &llm, None).await;
    let session = komo_gateway::service::test_support::driver_stream::open_session(&gateway).await;
    let accepted = komo_gateway::service::test_support::driver_stream::submit(
        &gateway, &session, "obs-7", TASK,
    )
    .await;
    gateway.wait_run(&accepted.run).await;

    let seen = recording.seen();
    assert_eq!(seen.len(), 4);
    let full = seen[1]
        .tool_results
        .iter()
        .find(|(id, _)| id == "pc-big")
        .map(|(_, text)| text.clone())
        .expect("read 的结果");
    assert!(
        full.len() > 4096 && !full.contains("这份结果前文已完整给过"),
        "{full}"
    );
    for (index, request) in seen.iter().enumerate().skip(1) {
        let big = request
            .tool_results
            .iter()
            .find(|(id, _)| id == "pc-big")
            .map(|(_, text)| text.as_str());
        assert_eq!(big, Some(full.as_str()), "第 {index} 次请求不改前缀");
    }
    assert!(decayed_decisions(&gateway.events(&session).await).is_empty());
    gateway.stop().await;

    let llm2 = FakeLlm::finisher("好。");
    let (mut gateway, recording2) = recorded(&home, &config, &llm2, None).await;
    let accepted = komo_gateway::service::test_support::driver_stream::submit(
        &gateway,
        &session,
        "obs-7b",
        "再说一句",
    )
    .await;
    gateway.wait_run(&accepted.run).await;
    let first = recording2.seen().remove(0);
    let replayed = first
        .tool_results
        .iter()
        .find(|(id, _)| id == "pc-big")
        .map(|(_, text)| text.clone())
        .expect("回放里有那条 read 结果");
    assert_eq!(replayed, full, "回放仍是活着时给过的完整正文");
    assert!(decayed_decisions(&gateway.events(&session).await).is_empty());
    gateway.stop().await;
}
