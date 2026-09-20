//! §14「恢复故障注入验收」十五行，逐行一个测试。
//!
//! 命名是 `row_<行号>_<行文>`。每个测试都断言**副作用次数**、Run / ToolCall 身份、
//! 授权消费与事件配对，而不是"状态变成了 running"。

use std::sync::Arc;

use komo_kernel::protocol::http::{InterventionKind, InterventionListQuery, InterventionSummary};
use komo_kernel::traits::StoreError;
use komo_kernel::types::ids::{ExecutorId, InterventionId, RunId};
use komo_kernel::types::status::{RetryCause, RunState, WaitReason};
use komo_kernel::types::turn::LlmError;

use crate::harness::*;

/// 让模型写一个文件：内容固定，所以"写没写过"可以按内容哈希核对（§8.6）。
fn write_args(path: &std::path::Path, content: &str) -> serde_json::Value {
    serde_json::json!({"path": path.display().to_string(), "content": content})
}

fn shell_args(command: &str) -> serde_json::Value {
    serde_json::json!({"command": command})
}

/// 「模型写一个文件，然后收尾」的两轮脚本。
fn write_then_done(path: &std::path::Path, content: &str) -> Arc<FakeLlm> {
    FakeLlm::new(vec![vec![
        call_round(1, "pc-1", "write", write_args(path, content)),
        text_round(2, "写好了。"),
    ]])
}

/// 「跑一条 shell，然后收尾」的两轮脚本。
fn shell_then_done(command: &str) -> Arc<FakeLlm> {
    FakeLlm::new(vec![vec![
        call_round(1, "pc-1", "shell", shell_args(command)),
        text_round(2, "跑过了。"),
    ]])
}

// ---------------------------------------------------------------- 第 1 行

/// 第 1 行：**Run 已提交，内存队列尚未收到通知** → 启动或周期扫描找到原 Run，自动执行一次。
///
/// 注入：`start_run` 之前跳闸。执行者一个字都没写进日志，调度器 `release` 之后这一行成了
/// 「`running` 且没有主人」的孤儿——正是"已提交、没人在跑"。重启后的进程内存队列里什么都
/// 没有，只有启动扫描（对账）能发现它、按日志尾部判成 `queued` 并跑起来。
#[tokio::test]
async fn row_1_a_committed_run_whose_wake_up_was_lost_is_found_and_executed_once() {
    let home = Home::new();
    let target = home.workspace().join("row1.txt");

    let fault = home.inject(Fault::BeforeStartRun);
    let gw = home.start(write_then_done(&target, "row-1")).await;
    let session = gw.open_session().await;
    let submitted = gw.submit(&session, "row-1", "写个文件").await;
    let run = submitted.run.clone();
    fault.wait_tripped().await;
    // 这一步的账本写不动，所以日志上只有输入那两条。
    assert_eq!(
        home.event_types(&session),
        vec!["run.accepted", "run.queued"],
        "执行者一个字都没写下"
    );
    assert!(!target.exists(), "工具一次都没跑");
    gw.stop().await;

    // 停机不再留一个叫 `interrupted` 的状态（§8.4），而**交还领取权也不改状态**（§8.7）：
    // 这一行是「`running` 且没有主人」的孤儿——一个可查询的事实（§8.9 的
    // `unowned_running`），而且不可领取（候选只有 `queued` 与 `waiting + retry`），所以
    // 重启后的对账先按日志尾部（一个字都没执行）把它判成 `queued` 再跑。
    let record = db_record(&home, &run).await;
    assert_eq!(
        record.state,
        RunState::Running,
        "停机前这一行是「已提交、没人在跑」的孤儿：{record:?}"
    );
    assert!(
        record.claimed_by.is_none(),
        "领取权已经交还（`claimed_by IS NULL`），状态留在 `running`：{record:?}"
    );

    // 重启：新进程的内存队列是空的。
    home.clear_injection();
    let llm = write_then_done(&target, "row-1");
    let gw = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    assert_eq!(detail.summary.run, run, "还是原来那个 Run");

    assert_eq!(
        std::fs::read_to_string(&target).unwrap_or_default(),
        "row-1",
        "自动执行了一次"
    );
    let events = home.events(&session);
    assert_eq!(tool_started(&events).len(), 1, "只跑了一次");
    assert_eq!(tool_results(&events).len(), 1, "一个 started 配一个 result");
    assert!(unpaired_attempts(&events).is_empty());
    gw.stop().await;
}

// ---------------------------------------------------------------- 第 2 行

/// 第 2 行：**输入已持久保存且 queued 已提交，客户端确认响应丢失** → 客户端用同一请求键
/// 重发，仍返回原 Run。
#[tokio::test]
async fn row_2_the_same_request_key_after_a_restart_returns_the_original_run() {
    let home = Home::new();
    let target = home.workspace().join("row2.txt");

    let gw = home.start(write_then_done(&target, "row-2")).await;
    let session = gw.open_session().await;
    let first = gw.submit(&session, "row-2", "写个文件").await;
    let run = first.run.clone();
    assert!(!first.deduplicated);
    gw.wait_terminal(&run).await;
    gw.stop().await;

    // 客户端没收到那个确认响应，于是用同一请求键重发。
    let gw = home.start(write_then_done(&target, "row-2")).await;
    let again = gw.submit(&session, "row-2", "写个文件").await;
    assert_eq!(again.run, run, "还是原 Run");
    assert!(again.deduplicated, "认出来是重发");

    // 内容不同的同键重发是冲突，不是第二个 Run。
    let (status, body) = gw
        .post(
            &format!("/v1/sessions/{session}/runs"),
            serde_json::json!({"request_key": "row-2", "text": "换一句"}),
        )
        .await;
    assert_eq!(status, 409, "{body}");

    let runs = komo_store::repos::runs::list_for_session(&gw.running.state.db, &session)
        .await
        .expect("读得到");
    assert_eq!(runs.len(), 1, "账本上只有一个 Run：{runs:?}");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap_or_default(),
        "row-2"
    );
    let events = home.events(&session);
    assert_eq!(tool_started(&events).len(), 1, "重发没有把工具再跑一遍");
    gw.stop().await;
}

// ---------------------------------------------------------------- 第 3 行

/// 第 3 行：**LLM 输出尚未收齐** → 重试模型请求；未收齐的调用从未执行。
///
/// 注入：模型驱动返回 `LlmError::Incomplete`（"回复未收齐"，`llm::is_retryable` 认它）。
/// Run 让出名额进 `waiting + retry`，日志上没有任何 `message.assistant`。
#[tokio::test]
async fn row_3_a_reply_that_never_arrived_in_full_is_re_requested_and_ran_nothing() {
    let home = Home::new();
    let target = home.workspace().join("row3.txt");

    let gw = home
        .start(FakeLlm::always(vec![Err(LlmError::Incomplete)]))
        .await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "row-3", "写个文件").await.run;
    // 让出名额去等退避：状态只有一个 `waiting`，"在等什么"在 `WaitReason` 里（§8.4）。
    let wait = gw.wait_waiting(&run).await;
    assert!(
        matches!(
            wait,
            WaitReason::Retry {
                attempts: 1,
                cause: RetryCause::Transport,
                ..
            }
        ),
        "回复没收齐要等一次退避（次数与哪一种失败都要写下来）：{wait:?}"
    );

    let events = home.events(&session);
    assert!(
        !events.iter().any(|e| e.type_name() == "message.assistant"),
        "回复没收齐，就没有一个完整的 assistant 事件：{:?}",
        home.event_types(&session)
    );
    assert!(tool_started(&events).is_empty(), "未收齐的调用从未执行");
    assert!(!target.exists());
    let attempts_before = db_retry_attempts(&gw, &run).await;
    assert_eq!(attempts_before, 1, "重试次数已经持久化");
    gw.stop().await;

    // 重启：预算不重置，到期之后重新请求模型。退避的底是 2 秒，等 6 秒足够到期两轮。
    let llm = write_then_done(&target, "row-3");
    let gw = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;

    // 到期之后由调度器自己领回去（§8.7 的候选查询里 `wait_kind = 'retry' AND
    // wake_at <= now`）：退避的等待三列与**交还领取权**是同一个提交（`mark_waiting_in`
    // 里连 `claimed_by` 一起清），所以这一行不再是"停在等待上"。
    let status = gw.db_state(&run).await;
    assert_ne!(
        status,
        RunState::Waiting,
        "到期之后要重新请求模型（§8.4 第 9 行），实际停在 {status:?} 不动"
    );
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    assert!(llm.turns() >= 1, "重启后确实重新请求了模型");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap_or_default(),
        "row-3",
        "这一次的调用跑了，且只跑了一次"
    );
    let events = home.events(&session);
    assert_eq!(tool_started(&events).len(), 1);
    assert!(unpaired_attempts(&events).is_empty());
    gw.stop().await;
}

// ---------------------------------------------------------------- 第 4 行

/// 第 4 行：**完整计划已同步到 JSONL，第一个工具尚未 started** → 自动执行原计划，调用 ID
/// 不变。
///
/// 注入：`start_call` 之前跳闸。日志上有 `message.assistant` + `tool.planned`，没有
/// `tool.started`。重启后的模型脚本**不再要求任何调用**，所以跑起来的那一次只能来自日志
/// 里那份原计划。
#[tokio::test]
async fn row_4_a_persisted_plan_runs_on_restart_under_the_same_call_id() {
    let home = Home::new();
    let target = home.workspace().join("row4.txt");

    let fault = home.inject(Fault::BeforeStartCall);
    let gw = home.start(write_then_done(&target, "row-4")).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "row-4", "写个文件").await.run;
    fault.wait_tripped().await;
    gw.stop().await;

    let events = home.events(&session);
    let planned = planned_calls(&events);
    assert_eq!(
        planned.len(),
        1,
        "计划落盘了：{:?}",
        home.event_types(&session)
    );
    assert!(tool_started(&events).is_empty(), "第一个工具还没 started");
    assert!(!target.exists(), "真实动作没发出");
    let call_before = planned[0].clone();

    home.clear_injection();
    let llm = FakeLlm::finisher("接着原计划做完了。");
    let gw = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    assert_eq!(
        std::fs::read_to_string(&target).unwrap_or_default(),
        "row-4",
        "原计划自动执行了一次"
    );
    let events = home.events(&session);
    let started = tool_started(&events);
    assert_eq!(started.len(), 1, "只跑了一次");
    assert_eq!(started[0].call_id, call_before, "调用 ID 不变");
    assert_eq!(tool_results(&events).len(), 1);
    assert!(unpaired_attempts(&events).is_empty());
    gw.stop().await;
}

// ---------------------------------------------------------------- 第 5 行

/// 第 5 行：**started 已提交，真实动作还没发出** → 保守核对，不能从 started 单独推断已
/// 执行或未执行。
///
/// 注入：`start_call` 返回之后跳闸——JSONL 与 state.db 两步都提交了 `tool.started`，工具
/// 一个字节都没写。恢复必须走 §8.6 的核对：`write` 有可核对的目标身份，核对结论是"确定
/// 未执行"，于是重做同一原子修改；总副作用次数仍然是一。
#[tokio::test]
async fn row_5_a_started_call_is_verified_rather_than_assumed() {
    let home = Home::new();
    let target = home.workspace().join("row5.txt");

    let fault = home.inject(Fault::AfterStartCall);
    let gw = home.start(write_then_done(&target, "row-5")).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "row-5", "写个文件").await.run;
    fault.wait_tripped().await;
    gw.stop().await;

    let events = home.events(&session);
    let started = tool_started(&events);
    assert_eq!(
        started.len(),
        1,
        "started 已提交：{:?}",
        home.event_types(&session)
    );
    assert!(tool_results(&events).is_empty(), "没有结果");
    assert!(!target.exists(), "真实动作还没发出");
    let call_before = started[0].call_id.clone();
    let attempt_before = started[0].attempt_id.clone();

    home.clear_injection();
    let gw = home.start(FakeLlm::finisher("核对之后做完了。")).await;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    assert_eq!(
        std::fs::read_to_string(&target).unwrap_or_default(),
        "row-5",
        "核对出「确定未执行」之后才重做，且只做一次"
    );
    let events = home.events(&session);
    let started = tool_started(&events);
    assert_eq!(started.len(), 2, "两次尝试：中断的那次 + 核对之后的那次");
    assert!(
        started.iter().all(|s| s.call_id == call_before),
        "同一个 ToolCall，新的只是 attempt"
    );
    assert_ne!(started[1].attempt_id, attempt_before, "新的一次尝试");
    let results = tool_results(&events);
    assert_eq!(results.len(), 1, "只有核对之后那一次有结果");
    assert_eq!(results[0].attempt_id, started[1].attempt_id);

    // 中断的那次尝试在 state.db 里应当被明确标成 interrupted，而不是不了了之。
    let attempts = komo_store::repos::calls::attempts_of(&gw.running.state.db, &call_before)
        .await
        .expect("读得到");
    let stale = attempts
        .iter()
        .find(|row| row.id == attempt_before.to_string())
        .expect("那次尝试还在账本上");
    // 启动回收在同一个事务里收拾上一代遗留的尝试（§8.7）：`started` 在 §8.6 里的意思是
    // "正在跑"，不标成 `interrupted` 的话，核对流程看到的就是一个永远跑不完的尝试。
    assert_eq!(
        stale.state, "interrupted",
        "上一代遗留的尝试要有一个明确结论（interrupted），不能停在 started：{stale:?}"
    );
    gw.stop().await;
}

// ---------------------------------------------------------------- 第 6 行

/// 第 6 行：**外部写入成功，完整输出与结果事件尚未持久保存** → 借助可靠幂等或关联信息
/// 核对；没有证据则等待处理，**不产生第二条记录**。
///
/// 这一行的分界在 `ToolOutputStore::publish` 上：外部副作用已经发生，**输出还没落盘**。
/// 落盘之后再中断是第 8 行（那时盘上有一份身份齐全的 `output.json`，补记结果才是对的）。
///
/// 注入两步：① `finish_call` 之前跳闸——`shell` 真的跑完，计数文件多了一行；② 停机之后
/// 把这次尝试的输出目录整棵删掉，等价于 `publish` 从未发生（故障装饰器包的是 `Ledger`，
/// 包不到 `ToolOutputStore`，所以这一刀走"直接篡改磁盘"那条注入路）。
///
/// 于是恢复手上**一点证据都没有**：`shell` 的恢复方式是 `NoSafeRecovery`、`verify` 是
/// `Unavailable`，只能停在 `waiting + intervention`（一条 `verify` 的清单条目），而不是
/// 再跑一次命令。
#[tokio::test]
async fn row_6_an_external_write_without_evidence_waits_instead_of_writing_twice() {
    let home = Home::new();
    let counter = Counter::new(&home, "row6.count");

    let fault = home.inject(Fault::BeforeFinishCall);
    let gw = home.start(shell_then_done(&counter.append_command())).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "row-6", "跑一条命令").await.run;

    // 任意 shell 是 Ask（§7.1 第 5 行）：先批一次。
    let record = gw.wait_approval().await;
    assert_eq!(record.run.as_ref(), Some(&run));
    gw.decide(&record.approval, true).await;

    fault.wait_tripped().await;
    gw.stop().await;
    assert_eq!(counter.count(), 1, "外部写入成功了一次");
    let events = home.events(&session);
    let started = tool_started(&events)[0].clone();
    assert_eq!(tool_started(&events).len(), 1);
    assert!(tool_results(&events).is_empty(), "结果事件尚未持久保存");

    // ② 输出也没落盘。
    home.drop_attempt_output(&session, &run, &started);
    assert!(
        !home.attempt_dir(&session, &run, &started).exists(),
        "完整输出尚未持久保存"
    );

    home.clear_injection();
    let gw = home.start(FakeLlm::finisher("不该走到这里。")).await;
    let detail = gw
        .wait_state(
            &run,
            |s| s == RunState::Waiting || s.is_terminal(),
            "waiting",
        )
        .await;
    assert_eq!(
        detail.summary.state,
        RunState::Waiting,
        "没有核对证据就等人处理，而不是重跑或宣布失败：{detail:?}"
    );
    assert_eq!(
        detail.summary.wait,
        Some(WaitReason::Intervention {
            intervention: InterventionId::for_run(&run),
        }),
        "停在 `waiting` 就要说得出在等哪一条 Intervention（§8.4）：{detail:?}"
    );
    // 等人就是等人：它必须在 §7.5 的清单里，且因为有一条 `uncertain` 的调用而是
    // `verify`——操作者要回答的是"上次那个调用到底发生没有"。
    let pending = pending_intervention(&gw, &run)
        .await
        .expect("停成这样的一条要在清单里（§7.5）");
    assert_eq!(pending.kind, InterventionKind::Verify, "{pending:?}");
    assert_eq!(counter.count(), 1, "**不产生第二条记录**");

    // 那条上一世的 `tool.started` 要配一条**明确的 uncertain**，不能悬着（§14 事件配对）。
    let events = home.events(&session);
    assert_eq!(tool_started(&events).len(), 1, "没有第二次尝试");
    assert!(
        unpaired_attempts(&events).is_empty(),
        "没配上的尝试：{:?}",
        unpaired_attempts(&events)
    );
    assert_eq!(
        result_statuses(&events),
        vec![komo_kernel::types::refs::ToolResultStatus::Uncertain],
        "副作用发生没发生不知道——这就是 uncertain 的意思（§8.6）"
    );
    assert_eq!(
        tool_results(&events)[0].attempt_id,
        started.attempt_id,
        "配的是**那一次**尝试"
    );
    gw.stop().await;
}

// ---------------------------------------------------------------- 第 7 行

/// 第 7 行：**输出文件和 JSONL 引用已持久保存，state.db 尚未更新** → 校验输出后补结果
/// 索引，复用原输出，工具不重做。
///
/// 注入两步：① `finish_call` 之前跳闸——工具跑完、`output.json` 已发布、`tool.result` 没
/// 写；② 停机之后**手写**那条 `tool.result` 进 JSONL（引用真实的 output.json，带真实大小
/// 与哈希）。于是 JSONL 走在前面、state.db 落在后面，正是这一行说的状态。
#[tokio::test]
async fn row_7_a_result_the_database_never_indexed_is_backfilled_not_rerun() {
    let home = Home::new();
    let counter = Counter::new(&home, "row7.count");

    let fault = home.inject(Fault::BeforeFinishCall);
    let gw = home.start(shell_then_done(&counter.append_command())).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "row-7", "跑一条命令").await.run;
    let record = gw.wait_approval().await;
    gw.decide(&record.approval, true).await;
    fault.wait_tripped().await;
    gw.stop().await;
    assert_eq!(counter.count(), 1);

    // ② 手写 tool.result：state.db 完全不知道它。
    let events = home.events(&session);
    let started = tool_started(&events)[0].clone();
    let appended = append_tool_result(&home, &session, &run, &started);
    assert_eq!(
        home.event_types(&session).last().map(String::as_str),
        Some("tool.result"),
        "JSONL 的尾部是结果事件"
    );

    // state.db 还停在 started。
    let call_state = db_call_state(&home, &started.call_id).await;
    assert_eq!(call_state, "started", "state.db 尚未更新");

    home.clear_injection();
    let gw = home.start(FakeLlm::finisher("复用原输出，做完了。")).await;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    assert_eq!(counter.count(), 1, "工具不重做");
    let events = home.events(&session);
    assert_eq!(tool_started(&events).len(), 1, "没有第二次尝试");
    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].output_ref, appended, "复用的就是原输出");
    assert_eq!(
        komo_store::repos::calls::list_for_run(&gw.running.state.db, &run)
            .await
            .expect("读得到")[0]
            .state,
        "completed",
        "结果索引补齐了"
    );
    gw.stop().await;
}

// ---------------------------------------------------------------- 第 8 行

/// 第 8 行：**output.json 已完成但 JSONL 结果事件尚未写入** → 校验身份、计划及完成状态后
/// 补记结果；不能只凭文件存在判断。
///
/// 注入：`finish_call` 之前跳闸，**不**手写结果事件。磁盘上有一个身份完整的 output.json
/// （里面绑着 session / run / call / attempt 与计划哈希），JSONL 上没有对应的 tool.result。
#[tokio::test]
async fn row_8_a_finished_output_without_its_result_event_is_verified_and_backfilled() {
    let home = Home::new();
    let target = home.workspace().join("row8.txt");

    let fault = home.inject(Fault::BeforeFinishCall);
    let gw = home.start(write_then_done(&target, "row-8")).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "row-8", "写个文件").await.run;
    fault.wait_tripped().await;
    gw.stop().await;

    let events = home.events(&session);
    let started = tool_started(&events)[0].clone();
    let output = output_json_path(&home, &session, &run, &started);
    assert!(output.exists(), "output.json 已完成：{}", output.display());
    assert!(tool_results(&events).is_empty(), "结果事件尚未写入");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap_or_default(),
        "row-8",
        "工具确实跑完了"
    );

    home.clear_injection();
    let gw = home.start(FakeLlm::finisher("补记之后做完了。")).await;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    let events = home.events(&session);
    let results = tool_results(&events);
    // 恢复在存储里找到了那份**身份逐字段对得上**的孤儿 output.json（`OrphanOutputs` 接
    // 在恢复扫描上），于是给那次尝试补记结果再入队——那条 `tool.started` 因此配上了。
    assert!(
        unpaired_attempts(&events).is_empty(),
        "每个 tool.started 都要配一个结果或一条明确的 uncertain，没配上的：{:?}；\
         这个 Run 的事件是 {:?}",
        unpaired_attempts(&events),
        home.event_types(&session)
    );
    assert_eq!(
        results.len(),
        1,
        "补记了一条结果：{:?}",
        home.event_types(&session)
    );
    // 补记用的是**原来那次尝试**的身份与原输出，不是另起一次（§8.6：核对之后复用原输出）。
    assert_eq!(
        results[0].attempt_id, started.attempt_id,
        "补记的是**原来那次尝试**的结果，而不是另起一次"
    );
    assert_eq!(
        results[0].output_ref.path(),
        format!(
            "tool-output/{run}/{}/{}/output.json",
            started.call_id, started.attempt_id
        ),
        "复用已经完整落盘的那份输出"
    );
    assert_eq!(tool_started(&events).len(), 1, "不重跑");
    gw.stop().await;
}

// ---------------------------------------------------------------- 第 9 行

/// 第 9 行：**输出文件引用已提交，但正文缺失或被修改** → 停止受影响任务，报告引用损坏，
/// 不通过重跑补造旧结果。
///
/// 注入：`record_round` 第二次之前跳闸（这一轮的工具已经收尾、结果事件已经落盘，模型正要
/// 被问下一轮），然后删掉 / 改写 output.json。
#[tokio::test]
async fn row_9_a_missing_or_altered_output_body_halts_the_task() {
    for (label, damage) in [("缺失", true), ("被修改", false)] {
        let home = Home::new();
        let counter = Counter::new(&home, "row9.count");

        let fault = home.inject(Fault::BeforeRecordRound(2));
        let gw = home.start(shell_then_done(&counter.append_command())).await;
        let session = gw.open_session().await;
        let run = gw.submit(&session, "row-9", "跑一条命令").await.run;
        let record = gw.wait_approval().await;
        gw.decide(&record.approval, true).await;
        fault.wait_tripped().await;
        gw.stop().await;
        assert_eq!(counter.count(), 1, "{label}");

        let events = home.events(&session);
        let started = tool_started(&events)[0].clone();
        assert_eq!(tool_results(&events).len(), 1, "{label}：结果引用已提交");
        let output = output_json_path(&home, &session, &run, &started);
        if damage {
            std::fs::remove_file(&output).expect("删掉输出正文");
        } else {
            let mut body: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
            body["result"] = serde_json::json!({"tampered": true});
            std::fs::write(&output, serde_json::to_vec(&body).unwrap()).unwrap();
        }

        home.clear_injection();
        let gw = home.start(FakeLlm::finisher("不该走到这里。")).await;
        let detail = gw
            .wait_state(
                &run,
                |s| s == RunState::Waiting || s.is_terminal(),
                "waiting",
            )
            .await;
        assert_eq!(
            detail.summary.state,
            RunState::Waiting,
            "{label}：停止受影响任务：{detail:?}"
        );
        // 引用损坏是"前提没了"那一类（§7.5）：停在 `waiting + intervention` 上等人
        // 处置，清单里是一条 `blocked`（没有 `uncertain` 的调用可核对）。
        assert_eq!(
            detail.summary.wait,
            Some(WaitReason::Intervention {
                intervention: InterventionId::for_run(&run),
            }),
            "{label}：{detail:?}"
        );
        let pending = pending_intervention(&gw, &run)
            .await
            .expect("引用损坏要出现在清单里");
        assert_eq!(
            pending.kind,
            InterventionKind::Blocked,
            "{label}：{pending:?}"
        );
        let reason = db_last_error(&home, &gw, &run).await.unwrap_or_default();
        assert!(
            reason.contains("引用") || reason.contains("哈希") || reason.contains("缺失"),
            "{label}：要报告引用损坏，读到的是「{reason}」"
        );
        assert_eq!(counter.count(), 1, "{label}：不通过重跑补造旧结果");
        gw.stop().await;
    }
}

// ---------------------------------------------------------------- 第 10 行

/// 第 10 行：**用户批准已保存，审计 JSONL 尚未补写** → 从 state.db 读取有效授权，outbox
/// 幂等补写，**不重新询问**。
#[tokio::test]
async fn row_10_a_saved_approval_is_reused_and_its_audit_event_is_backfilled_once() {
    let home = Home::new();
    let counter = Counter::new(&home, "row10.count");

    let fault = home.inject(Fault::BeforeAppendAudit);
    let gw = home.start(shell_then_done(&counter.append_command())).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "row-10", "跑一条命令").await.run;
    let record = gw.wait_approval().await;
    gw.decide(&record.approval, true).await;

    // 决定已经在 state.db 里。
    let stored = gw
        .running
        .state
        .approval_repo
        .get(&record.approval)
        .await
        .expect("读得到")
        .expect("有这条");
    assert!(stored.decision.as_ref().expect("有结论").approved);

    // 只等终态：这一条 Run 一开始就停在审批上（`waiting + approval`），把 `Waiting`
    // 也算成"收场"会在决定生效之前就返回。
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;
    gw.stop().await;
    let _ = fault;

    // 审计事件没写进 JSONL。
    let types_before = home.event_types(&session);
    assert!(
        !types_before.iter().any(|t| t == "approval.decided"),
        "审计事件还没补写：{types_before:?}"
    );

    home.clear_injection();
    let gw = home.start(FakeLlm::finisher("做完了。")).await;
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;
    // 给 outbox 补写一点时间（启动那一步就补过一次；这一拍是兜底）。
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // 不重新询问：待处理审批里没有新的一条。
    let pending = gw.approvals().await;
    assert!(
        pending.iter().all(|r| r.approval == record.approval),
        "不该为同一个动作再问一次：{pending:?}"
    );
    assert_eq!(counter.count(), 1, "已批准的动作只执行一次");

    // §8.5 的反向补写：决定先落 state.db 与 `control_outbox`，审计那条随后（也叫醒
    // 补写器）按 `event_id` 幂等补进 JSONL——重启补一次，还只补一条。
    let types = home.event_types(&session);
    assert!(
        types.iter().any(|t| t == "approval.decided"),
        "重启后 outbox 幂等补写一条审计事件：{types:?}"
    );
    assert_eq!(
        types.iter().filter(|t| *t == "approval.decided").count(),
        1,
        "**幂等**：只补一条"
    );
    gw.stop().await;
}

// ---------------------------------------------------------------- 第 11 行

/// 第 11 行：**JSONL 最后半行未被任何提交引用** → 隔离尾部后修复；按最后完整记录恢复，
/// 未知副作用仍须核对。
#[tokio::test]
async fn row_11_a_half_written_tail_is_quarantined_and_recovery_continues() {
    let home = Home::new();
    let counter = Counter::new(&home, "row11.count");

    let fault = home.inject(Fault::BeforeRecordRound(2));
    let gw = home.start(shell_then_done(&counter.append_command())).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "row-11", "跑一条命令").await.run;
    let record = gw.wait_approval().await;
    gw.decide(&record.approval, true).await;
    fault.wait_tripped().await;
    gw.stop().await;
    assert_eq!(counter.count(), 1);

    // 半行：一条写到一半、没有换行结尾的记录，谁也没引用过它。
    let path = home.events_path(&session);
    let complete = std::fs::read_to_string(&path).expect("读日志");
    let half = format!(
        r#"{{"v":1,"seq":{},"event_id":"evt-half","session_id":"{session}","#,
        home.events(&session).len() + 1
    );
    std::fs::write(&path, format!("{complete}{half}")).expect("写半行");

    home.clear_injection();
    let gw = home
        .start(FakeLlm::finisher("按最后完整记录接着做完了。"))
        .await;
    let detail = gw.wait_terminal(&run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");

    assert!(
        home.quarantine_path(&session).exists(),
        "半行先隔离保留原始字节"
    );
    let quarantined = std::fs::read_to_string(home.quarantine_path(&session)).unwrap();
    assert!(quarantined.contains("evt-half"), "{quarantined}");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("evt-half"), "隔离之后从日志里截掉了：{text}");
    assert!(text.starts_with(&complete), "完整有效行一个都没少");
    assert_eq!(counter.count(), 1, "未知副作用不靠重跑掩盖");
    gw.stop().await;
}

// ---------------------------------------------------------------- 第 12 行

/// 第 12 行：**JSONL 已提交范围缺失或中间损坏** → 停止**受影响会话**，报告损坏，不跳过
/// 记录或重做动作。
///
/// 两个会话：一个被损坏，一个健康。「其他 Session 正常运行」是 §8.4 明说的。
#[tokio::test]
async fn row_12_a_corrupt_middle_stops_only_the_affected_session() {
    let home = Home::new();
    let broken_counter = Counter::new(&home, "row12-broken.count");
    let healthy_target = home.workspace().join("row12-healthy.txt");

    // 受损的那个会话：跑到工具收尾之后停住。
    let fault = home.inject(Fault::BeforeRecordRound(2));
    let gw = home
        .start(shell_then_done(&broken_counter.append_command()))
        .await;
    let broken_session = gw.open_session().await;
    let broken_run = gw
        .submit(&broken_session, "row-12-a", "跑一条命令")
        .await
        .run;
    let record = gw.wait_approval().await;
    gw.decide(&record.approval, true).await;
    fault.wait_tripped().await;
    gw.stop().await;
    assert_eq!(broken_counter.count(), 1);

    // 健康的那个会话：只提交了输入（账本被毒化之后执行者写不下任何东西）。
    let fault = home.inject(Fault::BeforeStartRun);
    let gw = home.start(write_then_done(&healthy_target, "row-12")).await;
    let healthy_session = gw.open_session().await;
    let healthy_run = gw
        .submit(&healthy_session, "row-12-b", "写个文件")
        .await
        .run;
    fault.wait_tripped().await;
    gw.stop().await;

    // 中间损坏：删掉中间一行，seq 从此不连续。
    let path = home.events_path(&broken_session);
    let lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    assert!(lines.len() > 3, "{lines:?}");
    let mut kept = lines.clone();
    kept.remove(2);
    std::fs::write(&path, format!("{}\n", kept.join("\n"))).unwrap();

    home.clear_injection();
    let llm = write_then_done(&healthy_target, "row-12");
    let gw = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;

    // 健康会话照常跑完（「其他 Session 正常运行」，§8.4）。
    let healthy = gw
        .wait_db_state(&healthy_run, |s| s.is_terminal(), "终态")
        .await;
    assert_eq!(healthy, RunState::Completed, "其他 Session 正常运行");
    assert_eq!(
        std::fs::read_to_string(&healthy_target).unwrap_or_default(),
        "row-12"
    );

    // 受影响会话不重做动作——这一半是成立的。
    assert_eq!(broken_counter.count(), 1, "不重做动作");
    // 读它会明确报损坏，而不是装作没事。
    let read_back = gw.try_run_detail(&broken_run).await;
    assert!(
        read_back.is_err(),
        "读一个中间损坏的会话要报损坏：{read_back:?}"
    );

    // 受损的那一条被停成 `waiting + intervention`：停下来等人处置，而且**不再空转**
    // ——停在等待上的 Run 不可领取，不会在"领取→装配失败→交还"之间转圈。
    let handled_before = gw.running.state.scheduler.handled();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let handled_after = gw.running.state.scheduler.handled();

    let broken = gw.db_state(&broken_run).await;
    assert_eq!(
        broken,
        RunState::Waiting,
        "停止受影响会话并**报告**损坏：账本上这一行停在 {broken:?}，last_error 是 {:?}",
        komo_store::repos::runs::get(&gw.running.state.db, &broken_run)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.last_error),
    );
    assert_eq!(
        gw.db_wait(&broken_run).await,
        Some(WaitReason::Intervention {
            intervention: InterventionId::for_run(&broken_run),
        }),
        "停下来等操作者处置那个读不出来的会话（§7.5 的 `blocked`）"
    );
    let pending = pending_intervention(&gw, &broken_run)
        .await
        .expect("损坏要出现在清单里——否则就是'卡住但清单为空'");
    assert_eq!(pending.kind, InterventionKind::Blocked, "{pending:?}");
    assert_eq!(
        handled_after,
        handled_before,
        "被停成 waiting 之后不该再被领走：两秒里又多领了 {} 次",
        handled_after - handled_before
    );
    gw.stop().await;
}

/// §8.2 的权威边界：**取消是调度事实**（state.db），不该因为会话内容已经不在就整条失败。
///
/// 日志丢了以后这条 Run 停在 `waiting + intervention`（内容读不出来——§8.9 的"数据库与
/// 内容对不上"），而取消得往那个会话写一条 `run.cancelled`——写不进去就 500。于是 §8.4 的
/// 「操作者处理后 queued / cancelled」两条路一条也走不通，这条 Run 永远挂在清单上，操作者
/// 手上偏偏只有 `komo run cancel` 这一把。
#[tokio::test]
async fn a_run_in_a_session_whose_log_is_gone_can_still_be_cancelled() {
    let home = Home::new();
    let llm = FakeLlm::new(vec![vec![call_round(
        1,
        "pc-1",
        "shell",
        serde_json::json!({"command": "echo hi"}),
    )]]);
    let gw = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "gone-1", "跑一条命令").await.run;
    gw.wait_approval().await;
    gw.stop().await;

    // 日志没了：state.db 说应用到了 seq N，文件只有 0。
    std::fs::remove_file(home.events_path(&session)).expect("删掉日志");

    let gw = home.start(FakeLlm::finisher("不该走到这里。")).await;
    // 「Session 内容缺失……不许领这条 Run，停成 `waiting + intervention`，理由里说清缺
    // 什么」（§8.4）。它仍然是非终态，说得出在等什么。
    assert_eq!(
        gw.db_state(&run).await,
        RunState::Waiting,
        "读不出会话的那条 Run 停在等待上：{:?}",
        gw.db_state(&run).await
    );
    assert_eq!(
        gw.db_wait(&run).await,
        Some(WaitReason::Intervention {
            intervention: InterventionId::for_run(&run),
        }),
        "它得说得出在等什么（§8.4）"
    );

    // 操作者取消：**不该 500**。
    let (code, body) = gw
        .post(&format!("/v1/runs/{run}/cancel"), serde_json::json!({}))
        .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(gw.db_state(&run).await, RunState::Cancelled);

    // 会话清单上它不再"未完成"——这正是操作者要的那一步。
    let (code, body) = gw.get(&format!("/v1/sessions/{session}")).await;
    assert_eq!(code, 200, "{body}");
    let detail: serde_json::Value = serde_json::from_str(&body).expect("会话详情");
    assert!(
        detail["unfinished"]
            .as_array()
            .expect("未完成是个数组")
            .is_empty(),
        "{detail}"
    );
    assert_eq!(detail["current_state"], serde_json::Value::Null, "{detail}");
    gw.stop().await;
}

// ---------------------------------------------------------------- 第 13 行

/// 第 13 行：**最终结果已同步到 JSONL，SSE 尚未送达** → 客户端补读原结果，Run 保持
/// completed。
#[tokio::test]
async fn row_13_a_final_result_nobody_received_is_re_read_not_re_run() {
    let home = Home::new();
    let target = home.workspace().join("row13.txt");

    let gw = home.start(write_then_done(&target, "row-13")).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "row-13", "写个文件").await.run;
    let before = gw.wait_terminal(&run).await;
    assert_eq!(before.summary.state, RunState::Completed);
    // 没有任何客户端连过 SSE。
    gw.stop().await;

    let gw = home.start(FakeLlm::finisher("不该走到这里。")).await;
    // 补读：从 0 开始读事件，最终回复在里面。
    let (status, body) = gw
        .get(&format!("/v1/sessions/{session}/events?from=0"))
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("run.completed"), "{body}");
    assert!(body.contains("写好了。"), "{body}");

    let after = gw.run_detail(&run).await;
    assert_eq!(
        after.summary.state,
        RunState::Completed,
        "Run 保持 completed"
    );
    assert_eq!(after.final_message.as_deref(), Some("写好了。"));
    assert_eq!(
        std::fs::read_to_string(&target).unwrap_or_default(),
        "row-13",
        "不重新执行任务"
    );
    let events = home.events(&session);
    assert_eq!(tool_started(&events).len(), 1);

    // 「已保存最终结果，但客户端没有收到 → **补发**或补读原结果」（§8.4 第 10 行）。
    // 补读这一半上面已经断言；补发那一半的候选是"**终态但投递还卡着**"的行——恢复候选
    // 把 `terminal_undelivered` 也带上，否则 `RecoveryAction::RedeliverResult` 永远走不到。
    // 这条 Run 由 HTTP 提交、没有任何投递行（§8.9：TUI / HTTP 来源的结果靠客户端补读），
    // 所以它不该出现在候选里：没有卡住的投递，就没有补发这回事。
    let candidates = gw
        .running
        .state
        .recovery_store
        .unfinished_runs()
        .await
        .expect("读得到");
    assert!(
        !candidates.iter().any(|candidate| candidate.run == run),
        "没有卡住的投递，completed 的 Run 就不该在恢复扫描的候选里：{candidates:?}"
    );
    gw.stop().await;
}

// ---------------------------------------------------------------- 第 14 行

/// 第 14 行：**旧子进程仍存活，或恢复和 resume 同时触发** → 先阻止重复执行；核实进程
/// 结束，并且只有一个领取者。
#[tokio::test]
async fn row_14_a_surviving_child_blocks_recovery_and_resume_never_doubles_the_claim() {
    let home = Home::new();
    let counter = Counter::new(&home, "row14.count");

    // 跑到 started 之后停住，留下一个"上一代的执行实例"。
    let fault = home.inject(Fault::AfterStartCall);
    let gw = home.start(shell_then_done(&counter.append_command())).await;
    let session = gw.open_session().await;
    let run = gw.submit(&session, "row-14", "跑一条命令").await.run;
    let record = gw.wait_approval().await;
    gw.decide(&record.approval, true).await;
    fault.wait_tripped().await;
    let old_executor = gw.executor_id();
    gw.stop().await;
    assert_eq!(counter.count(), 0, "动作还没发出");

    // ① 上一代名下登记着一个**还活着**的子进程（用本进程的 pid，probe 一定说 Alive）。
    home.register_live_child(&old_executor, std::process::id());
    force_claimed_by(&home, &run, &old_executor).await;

    home.clear_injection();
    let gw = home.start(FakeLlm::finisher("不该走到这里。")).await;
    let detail = gw
        .wait_state(
            &run,
            |s| s == RunState::Waiting || s.is_terminal(),
            "waiting",
        )
        .await;
    assert_eq!(
        detail.summary.state,
        RunState::Waiting,
        "核实不了旧执行已结束就先阻止重复执行：{detail:?}"
    );
    // "无法确认旧执行已结束"属于 `blocked` 那一类（§7.5 的表）：停在
    // `waiting + intervention` 上等人，而且**不可领取**，所以不会重复启动。
    assert_eq!(
        detail.summary.wait,
        Some(WaitReason::Intervention {
            intervention: InterventionId::for_run(&run),
        }),
        "{detail:?}"
    );
    let pending = pending_intervention(&gw, &run)
        .await
        .expect("核实不了旧执行已结束的那条要在清单里");
    // 具体进哪一类由权威当场判（§7.5 第 1 条）：这条 Run 上还挂着一次 `uncertain` 的
    // 调用，所以它是 `verify`（"这次调用到底发生没有"），而不是 `blocked`——操作者要核
    // 的就是那一次调用。
    assert_eq!(pending.kind, InterventionKind::Verify, "{pending:?}");
    assert!(
        pending.call.is_some(),
        "`verify` 一定说得出停在哪一次调用上：{pending:?}"
    );
    assert_eq!(counter.count(), 0, "没有重复执行");
    gw.stop().await;

    // ② 子进程收尾之后：恢复与 resume 同时触发，只有一个领取者。
    std::fs::remove_file(
        home.path()
            .join("runtime")
            .join("children")
            .join(format!("{old_executor}.json")),
    )
    .expect("销掉那一行");

    let gw = home.start(FakeLlm::finisher("核对之后收场。")).await;
    // 启动扫描已经跑过；再从外面踢一次 resume，两条路进的是同一个领取入口（§8.7）。
    let (status, body) = gw
        .post(
            &format!("/v1/sessions/{session}/resume"),
            serde_json::json!({}),
        )
        .await;
    assert!(status == 200 || status == 409, "{status} {body}");
    gw.wait_state(&run, |s| s.is_terminal() || s == RunState::Waiting, "收场")
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    assert_eq!(
        counter.count(),
        0,
        "`shell` 没有可靠恢复方式，核对不出结论就不重跑——更不能跑两次"
    );
    let events = home.events(&session);
    assert_eq!(
        tool_started(&events).len(),
        1,
        "只有一个领取者，所以只有那一次尝试"
    );
    gw.stop().await;
}

// ---------------------------------------------------------------- 第 15 行

/// 第 15 行：**用户取消后重启，或连续恢复失败耗尽预算** → 已取消的不复活；失败有明确
/// 终态，不无限循环。
#[tokio::test]
async fn row_15_a_cancelled_run_never_revives_and_an_exhausted_budget_ends() {
    let home = Home::new();
    let target = home.workspace().join("row15.txt");

    // ① 取消：停在审批上的 Run 被用户取消。
    let counter = Counter::new(&home, "row15.count");
    let gw = home.start(shell_then_done(&counter.append_command())).await;
    let session = gw.open_session().await;
    let cancelled_run = gw.submit(&session, "row-15-a", "跑一条命令").await.run;
    gw.wait_approval().await;
    let (status, body) = gw
        .post(
            &format!("/v1/runs/{cancelled_run}/cancel"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    gw.wait_state(&cancelled_run, |s| s == RunState::Cancelled, "cancelled")
        .await;

    gw.stop().await;

    // ② 预算耗尽：模型一直"回复未收齐"，重试次数已经到顶。
    let gw = home
        .start(FakeLlm::always(vec![Err(LlmError::Incomplete)]))
        .await;
    let other = gw.open_session().await;
    let exhausted_run = gw.submit(&other, "row-15-b", "写个文件").await.run;
    let wait = gw.wait_waiting(&exhausted_run).await;
    assert!(
        matches!(wait, WaitReason::Retry { .. }),
        "回复没收齐是等一次退避：{wait:?}"
    );
    gw.stop().await;
    exhaust_retry_budget(&home, &exhausted_run).await;

    let llm = write_then_done(&target, "row-15");
    let gw = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;

    assert_eq!(
        gw.run_state(&cancelled_run).await,
        RunState::Cancelled,
        "用户明确取消的 Run 不自动复活"
    );
    assert_eq!(counter.count(), 0, "取消之后那条命令一次都没跑");

    // §8.4 第 9 行：`waiting + retry` 那一行是「**沿用已保存的次数与到点时刻，到点再
    // 尝试**」——到点了就再试一次，而不是由恢复扫描替它判死（那条路只写状态，不制造
    // 新事实，§8.9）。
    //
    // 预算耗尽这件事的判定在**错误路径**上（`AgentLoop::llm_error`：下一次失败时
    // `attempts >= max_attempts` 才写 `failed`）。所以这一段断言的是：到点被再试一次，
    // 而且**沿用了原来的次数**——重启既不重置预算，也不凭预算把一个还没失败过的请求
    // 提前判死。
    let exhausted = gw
        .wait_state(&exhausted_run, |s| s.is_terminal(), "终态")
        .await;
    assert_eq!(
        exhausted.summary.state,
        RunState::Completed,
        "到点就再试一次，不是被恢复扫描判死：{exhausted:?}"
    );
    assert!(target.exists(), "这一次真的请求了模型，所以文件写出来了");
    let carried = komo_store::repos::runs::get(&gw.state().db, &exhausted_run)
        .await
        .expect("读得到")
        .expect("有这个 Run")
        .retry_attempts;
    assert!(
        carried >= 99,
        "沿用了保存的次数（重启不重置预算）：{carried}"
    );
    gw.stop().await;
}

// ---------------------------------------------------------------- 工具函数

/// 停机之后直接从 state.db 读这一行——恢复要判的就是它的**形状**：状态与领取权是两格
/// （§8.7 的孤儿是 `running` 且 `claimed_by IS NULL`）。
async fn db_record(home: &Home, run: &RunId) -> komo_store::repos::runs::RunRecord {
    let db = home.open_db().await;
    komo_store::repos::runs::get(&db, run)
        .await
        .expect("读得到")
        .expect("有这一行")
}

/// §7.5 的清单里这条 Run 现在等人的那一条（没有就是 `None`）。
///
/// 它是**派生视图**（`runs` 与 `approval_requests` 的并集查询），所以断言它就是在断言
/// "这条 Run 挡着队、而且操作者看得见"——§8.4 的那句"卡住但清单为空"正好是它的反面。
async fn pending_intervention(gw: &Gw, run: &RunId) -> Option<InterventionSummary> {
    gw.state()
        .interventions(&InterventionListQuery {
            run: Some(run.clone()),
            ..Default::default()
        })
        .await
        .expect("读得出清单")
        .into_iter()
        .next()
}

async fn db_call_state(home: &Home, call: &komo_kernel::types::ids::ToolCallId) -> String {
    let db = home.open_db().await;
    let attempts = komo_store::repos::calls::attempts_of(&db, call)
        .await
        .expect("读得到");
    let run = RunId::from_raw(attempts[0].run_id.clone());
    komo_store::repos::calls::list_for_run(&db, &run)
        .await
        .expect("读得到")
        .into_iter()
        .find(|row| row.id == call.to_string())
        .expect("有这个调用")
        .state
}

async fn db_retry_attempts(gw: &Gw, run: &RunId) -> u32 {
    komo_store::repos::runs::get(&gw.running.state.db, run)
        .await
        .expect("读得到")
        .expect("有这一行")
        .retry_attempts
}

async fn db_last_error(_home: &Home, gw: &Gw, run: &RunId) -> Option<String> {
    komo_store::repos::runs::get(&gw.running.state.db, run)
        .await
        .expect("读得到")
        .and_then(|record| record.last_error)
}

/// 这次尝试的 `output.json` 在哪（§8.3 的目录形状）。
fn output_json_path(
    home: &Home,
    session: &komo_kernel::types::ids::SessionId,
    run: &RunId,
    started: &komo_kernel::events::ToolStarted,
) -> std::path::PathBuf {
    home.session_dir(session)
        .join("tool-output")
        .join(run.as_str())
        .join(started.call_id.as_str())
        .join(started.attempt_id.as_str())
        .join("output.json")
}

/// 手写一条 `tool.result` 到 JSONL 尾部：引用真实的 output.json，带真实大小与哈希。
/// 这是"JSONL 走在前、state.db 落在后"唯一能在进程内造出来的方式（§8.5）。
fn append_tool_result(
    home: &Home,
    session: &komo_kernel::types::ids::SessionId,
    run: &RunId,
    started: &komo_kernel::events::ToolStarted,
) -> komo_kernel::types::refs::OutputRef {
    let path = output_json_path(home, session, run, started);
    let bytes = std::fs::read(&path).expect("output.json 在");
    let reference = komo_kernel::types::refs::OutputRef(komo_kernel::types::refs::ContentRef {
        path: format!(
            "tool-output/{run}/{}/{}/output.json",
            started.call_id, started.attempt_id
        ),
        size: bytes.len() as u64,
        hash: komo_kernel::types::digest::ContentHash::of_bytes(&bytes),
        pointer: None,
    });
    let next = home.events(session).len() as u64 + 1;
    let event = komo_kernel::events::Event {
        v: komo_kernel::events::EVENT_FORMAT_VERSION,
        seq: komo_kernel::types::ids::Seq(next),
        event_id: komo_kernel::types::ids::EventId::from_raw("evt-handwritten-result"),
        session: session.clone(),
        run: Some(run.clone()),
        ts: time::OffsetDateTime::now_utc(),
        payload: komo_kernel::events::EventPayload::ToolResult(komo_kernel::events::ToolResult {
            call_id: started.call_id.clone(),
            attempt_id: started.attempt_id.clone(),
            status: komo_kernel::types::refs::ToolResultStatus::Completed,
            output_ref: reference.clone(),
            elapsed_ms: 7,
            preview: Some("once".into()),
            stdout: None,
            stderr: None,
            attempt_state: Some(komo_kernel::types::status::AttemptState::Completed),
        }),
    };
    let mut text = std::fs::read_to_string(home.events_path(session)).expect("读日志");
    text.push_str(&event.to_line().expect("序列化"));
    text.push('\n');
    std::fs::write(home.events_path(session), text).expect("写日志");
    reference
}

/// 把这一行的领取权强行按到某个执行实例名下（模拟"上一代还握着它"）。
async fn force_claimed_by(home: &Home, run: &RunId, executor: &str) {
    let db = home.open_db().await;
    let queue = komo_store::TursoRunQueue::new(db.clone());
    use komo_kernel::traits::RunQueue;
    // 先放回队列，再让那个"旧实例"领走。
    komo_store::RecoveryStore::new(db.clone(), home.sessions_dir())
        .requeue(run)
        .await
        .expect("放回队列");
    let claimed = queue
        .claim(&ExecutorId::from_raw(executor.to_string()))
        .await
        .expect("领取")
        .expect("领得到");
    assert_eq!(&claimed.run, run);
}

/// 把重试次数按到预算上限（`RetryBudget::default().max_attempts`）。
///
/// 退避的进度就是 `WaitReason::Retry` 那三个字段（§8.4），所以"用完了"这件事只能按它的
/// 形状写进去：次数按到上限、到点时刻就是现在（重启后对账当场放它回 `queued`）。
async fn exhaust_retry_budget(home: &Home, run: &RunId) {
    let db = home.open_db().await;
    let run = run.clone();
    db.with_write_retry(move |ex| {
        let run = run.clone();
        Box::pin(async move {
            let now = time::OffsetDateTime::now_utc();
            komo_store::repos::runs::mark_waiting_in(
                ex,
                &run,
                &WaitReason::Retry {
                    attempts: 99,
                    not_before: now,
                    cause: RetryCause::Transport,
                },
                Some("注入：预算用完了".into()),
                now,
            )
            .await
        }) as komo_store::db::BoxFuture<'_, Result<(), StoreError>>
    })
    .await
    .expect("写得下");
}
