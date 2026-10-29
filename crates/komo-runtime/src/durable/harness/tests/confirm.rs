//! 对外动作确认（`docs/confirm.md`）：无人值守的 Run 调对外工具，`prepare` 之后停在 `confirm`，
//! 批准了才提交意图执行；拒绝、到点按没执行收尾。有人值守的 Run、不对外的工具照旧直达。

use std::sync::Arc;

use komo_kernel::durable::TaskState;
use serde_json::{Value, json};

use super::super::{ConfirmAnswer, ConfirmVerdict, InputEntry, SubmissionDraft, confirm_address};
use super::support::*;
use crate::durable::Engine;

/// 由 Cron 起的 Run（第一条输入是 `cron.input`）。
async fn submit_cron(world: &World, engine: &Engine) -> komo_kernel::durable::SubmissionId {
    world
        .harness
        .submit(
            engine,
            CONV,
            SubmissionDraft::input("tick").with_entry(InputEntry::new("cron.input", None)),
        )
        .await
        .expect("submit")
}

fn phase(task: &komo_kernel::durable::TaskRecord) -> Option<String> {
    match &task.state {
        TaskState::Running { checkpoint } | TaskState::Pending { checkpoint } => {
            checkpoint["phase"].as_str().map(str::to_owned)
        }
        _ => None,
    }
}

/// 等工具任务停在 `confirm`，返回它。
async fn until_confirming(world: &World) -> komo_kernel::durable::TaskRecord {
    world
        .until(|| async {
            world
                .tasks("komo.tool")
                .await
                .iter()
                .any(|task| phase(task).as_deref() == Some("confirm"))
        })
        .await;
    world
        .tasks("komo.tool")
        .await
        .into_iter()
        .find(|task| phase(task).as_deref() == Some("confirm"))
        .unwrap()
}

async fn answer(engine: &Engine, task: komo_kernel::durable::TaskId, verdict: ConfirmVerdict) {
    let value = serde_json::to_value(ConfirmAnswer {
        verdict,
        by: "cli".into(),
        at: 0,
    })
    .unwrap();
    engine
        .commit(move |tx| {
            Box::pin(async move { tx.put_document(confirm_address(CONV, task), 1, value).await })
        })
        .await
        .unwrap();
}

async fn result_view(world: &World) -> (Value, String) {
    let entries = world.entries_of("komo.tool-result").await;
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    (
        entry.data.clone().unwrap(),
        text_of(&entry.model.as_ref().unwrap()[0]),
    )
}

fn outward_tool(world: &World) -> Arc<TestTool> {
    Arc::new(TestTool::writer("send", &world.log).outward())
}

#[tokio::test(start_paused = true)]
async fn a_cron_run_waits_for_approval_and_then_runs_the_outward_call_once() {
    let world = World::new();
    let send = outward_tool(&world);
    world.tools(vec![send.clone()]);
    world.script.calls(&[("c1", "send", json!({ "to": "x" }))]);
    world.script.reply("sent");
    let engine = world.open().await;
    let id = submit_cron(&world, &engine).await;

    let task = until_confirming(&world).await;
    assert_eq!(send.prepares.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(send.runs(), 0, "nothing runs before the answer");

    answer(&engine, task.id, ConfirmVerdict::Approve).await;
    world.harness.wait(&engine, id).await.unwrap();
    assert_eq!(send.runs(), 1);
    let (data, _) = result_view(&world).await;
    assert_eq!(data["status"], "completed");
    engine.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_rejected_outward_call_never_runs_and_the_model_is_told_so() {
    let world = World::new();
    let send = outward_tool(&world);
    world.tools(vec![send.clone()]);
    world.script.calls(&[("c1", "send", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = submit_cron(&world, &engine).await;

    let task = until_confirming(&world).await;
    answer(&engine, task.id, ConfirmVerdict::Reject).await;
    world.harness.wait(&engine, id).await.unwrap();
    assert_eq!(send.runs(), 0);
    let (data, view) = result_view(&world).await;
    assert_eq!(data["status"], "failed");
    assert!(
        view.contains("操作者拒绝了这次 send 调用，没有执行"),
        "{view}"
    );
    engine.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_outward_call_times_out_without_running() {
    let world = World::new();
    let send = outward_tool(&world);
    world.tools(vec![send.clone()]);
    world.script.calls(&[("c1", "send", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = submit_cron(&world, &engine).await;

    world.harness.wait(&engine, id).await.unwrap();
    assert_eq!(send.runs(), 0);
    let (_, view) = result_view(&world).await;
    // 测试替身的时限是 60 秒。
    assert!(view.contains("等了 1 分钟没有确认，没有执行"), "{view}");
    engine.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_restart_while_confirming_keeps_waiting_and_runs_after_approval() {
    let world = World::new();
    let send = outward_tool(&world);
    world.tools(vec![send.clone()]);
    world.script.calls(&[("c1", "send", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = submit_cron(&world, &engine).await;
    until_confirming(&world).await;

    let engine = world.restart(engine).await;
    let task = until_confirming(&world).await;
    assert_eq!(send.runs(), 0, "a restart neither runs nor skips the wait");
    assert_eq!(
        send.prepares.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the stored plan is used, not prepared again"
    );

    answer(&engine, task.id, ConfirmVerdict::Approve).await;
    world.harness.wait(&engine, id).await.unwrap();
    assert_eq!(send.runs(), 1);
    engine.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_attended_run_calls_an_outward_tool_directly() {
    let world = World::new();
    let send = outward_tool(&world);
    world.tools(vec![send.clone()]);
    world.script.calls(&[("c1", "send", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = world.submit(&engine, "send it").await;

    world.harness.wait(&engine, id).await.unwrap();
    assert_eq!(send.runs(), 1);
    engine.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_task_session_counts_as_unattended_even_for_typed_input() {
    let world = World::new();
    world.hooks.set_conversation(false);
    let send = outward_tool(&world);
    world.tools(vec![send.clone()]);
    world.script.calls(&[("c1", "send", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    world.submit(&engine, "send it").await;

    until_confirming(&world).await;
    assert_eq!(send.runs(), 0);
    engine.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_tool_not_declared_outward_runs_in_an_unattended_run() {
    let world = World::new();
    let bash = Arc::new(TestTool::writer("bash", &world.log));
    world.tools(vec![bash.clone()]);
    world.script.calls(&[("c1", "bash", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let id = submit_cron(&world, &engine).await;

    world.harness.wait(&engine, id).await.unwrap();
    assert_eq!(bash.runs(), 1);
    engine.close().await.unwrap();
}

/// 第一次请求里模型看得见的工具名（system entry 回放出的工具表）。
fn declared(world: &World) -> Vec<String> {
    crate::llm::replay_system(&world.script.requests()[0])
        .tools
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

/// 只在脚本里调的对外工具：无人值守的 Run 把它声明给模型（脚本里调会被拒，得在顶层调），
/// 有人值守的 Run 照旧不声明。
#[tokio::test(start_paused = true)]
async fn a_codemode_only_outward_tool_is_declared_only_to_unattended_runs() {
    for cron in [true, false] {
        let world = World::new();
        let send = Arc::new(TestTool {
            codemode_only: true,
            ..TestTool::writer("send", &world.log).outward()
        });
        let quiet = Arc::new(TestTool {
            codemode_only: true,
            ..TestTool::writer("quiet", &world.log)
        });
        world.tools(vec![send, quiet]);
        world.script.reply("ok");
        let engine = world.open().await;
        let id = if cron {
            submit_cron(&world, &engine).await
        } else {
            world.submit(&engine, "hi").await
        };
        world.harness.wait(&engine, id).await.unwrap();
        let names = declared(&world);
        assert_eq!(names.contains(&"send".to_owned()), cron, "{names:?}");
        assert!(!names.contains(&"quiet".to_owned()), "{names:?}");
        engine.close().await.unwrap();
    }
}
