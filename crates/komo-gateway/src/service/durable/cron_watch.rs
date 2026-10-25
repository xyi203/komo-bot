//! Durable 的 cron 看客（`docs/storage-durable-d5.md` §4 W3b）：一次触发投出去之后，等它的输入
//! submission 结算，按 **run 终态帧**（`komo.settled` 经 `legacy_events` 投影出的
//! `RunCompleted` / `RunFailed` / `RunCancelled`）决定回写什么触发状态、投不投。规则同
//! [`crate::service::cron_watch`]：`notify` 的 `always` / `on_error` / `never` 过滤**结果**；
//! 手动 run 没有触发记录，不回写。
//!
//! 与 Legacy 看客的区别：
//!
//! - 不订阅 hub，而是 `wait_submission` + 读终态帧——重启后 [`super::cron::recover`] 对仍是 `queued`
//!   的触发记录重挂，不会错过终态帧；
//! - 新语义没有"停在等人核对上"的 `waiting`（工具结果不明直接交给模型），所以没有 `Waiting`
//!   状态与"等待照投"那一支；faulted 任务的提醒是 W4a 的事；
//! - 命令型 Job（决定 9）不是 harness run：RunId 是 `shell.command` write 的 submission，输出从那条 entry 的
//!   `data.output` 引用读回 `BashResult`（与 Legacy 同样不信模型视图那份投影正文）。

use std::sync::Arc;

use komo_kernel::cron::FiringStatus;
use komo_kernel::durable::{WriteState, legacy_ids};
use komo_kernel::events::EventPayload;
use komo_kernel::traits::ToolOutputStore;
use komo_kernel::types::ids::{RunId, SessionId};
use komo_kernel::types::refs::OutputRef;
use komo_store::{FileToolOutputStore, SessionPaths};
use tracing::Instrument;

use super::feed;
use crate::service::cron_watch::{Watched, notify, settle};
use crate::service::state::GatewayState;

/// 盯一次触发，直到它有结论。同一个 Run 只盯一次。
pub fn watch(state: Arc<GatewayState>, watched: Watched) {
    let run = watched.run.clone();
    if !state.start_watching(&run) {
        tracing::debug!(%run, "这个 Run 已经有人在看了");
        return;
    }
    let span = tracing::info_span!(
        "komo.cron.firing",
        komo.cron.job = %watched.job,
        komo.cron.scheduled = watched.scheduled,
        komo.run.id = %run,
        komo.cron.outcome = tracing::field::Empty,
    );
    tokio::spawn(
        async move {
            follow(&state, &watched).await;
            state.stop_watching(&run);
        }
        .instrument(span),
    );
}

/// 一次触发的结论：`(状态, 错误, 投递正文, 要不要投)`。
struct Outcome {
    status: FiringStatus,
    error: Option<String>,
    text: String,
    deliver: bool,
}

async fn follow(state: &Arc<GatewayState>, watched: &Watched) {
    let Ok(core) = state.core() else { return };
    let Some(id) = legacy_ids::submission_of(&watched.run) else {
        tracing::warn!(run = %watched.run, "不是 Durable 的 RunId，cron 看客挂不上");
        return;
    };
    // 线封口（关闭）时报错：Gateway 在停，下一代的 `recover` 会再挂。
    if core.wait_submission(id).await.is_err() {
        return;
    }
    let outcome = if watched.is_command {
        command_outcome(state, &watched.session, id).await
    } else {
        run_outcome(state, &watched.session, &watched.run).await
    };
    finish(state, watched, outcome).await;
}

/// 模型 run：读 Session 的终态帧。
async fn run_outcome(state: &Arc<GatewayState>, session: &SessionId, run: &RunId) -> Outcome {
    let Ok(core) = state.core() else {
        return unreadable("没有 DurableCore");
    };
    let events = match feed::backfill(
        &*core.storage,
        session.as_str(),
        komo_kernel::types::ids::Seq::ZERO,
    )
    .await
    {
        Ok(events) => events,
        Err(error) => return unreadable(&error.to_string()),
    };
    for event in &events {
        if event.run.as_ref() != Some(run) {
            continue;
        }
        match &event.payload {
            EventPayload::RunCompleted(body) => {
                return Outcome {
                    status: FiringStatus::Ok,
                    error: None,
                    text: body
                        .final_message
                        .clone()
                        .unwrap_or_else(|| "（这一轮没有文字回复）".to_string()),
                    deliver: true,
                };
            }
            EventPayload::RunFailed(body) => {
                return Outcome {
                    status: FiringStatus::Error,
                    error: Some(body.reason.clone()),
                    text: format!("任务失败：{}", body.reason),
                    deliver: true,
                };
            }
            EventPayload::RunCancelled(_) => {
                return Outcome {
                    status: FiringStatus::Error,
                    error: Some("已取消".to_string()),
                    text: "任务已取消。".to_string(),
                    deliver: true,
                };
            }
            _ => {}
        }
    }
    unreadable("结算了但没有读到终态帧")
}

fn unreadable(why: &str) -> Outcome {
    Outcome {
        status: FiringStatus::Error,
        error: Some(why.to_string()),
        text: format!("读不出这次触发的结果（{why}），去原 Session 看细节（§10）"),
        deliver: true,
    }
}

/// 命令型 Job：
///
/// - 退出 0、stdout 非空：`Ok`，正文是 trim 过的 stdout，要投；
/// - 退出 0、stdout 为空：`Ok`，不投（看门狗"没事不说话"）；
/// - 非 0 退出：`Error`，正文是退出码 + stderr 末尾若干行，受 `notify` 过滤；
/// - 结果读不出来：留一句说得清出处的话，仍然投。
async fn command_outcome(
    state: &Arc<GatewayState>,
    session: &SessionId,
    submission: komo_kernel::durable::SubmissionId,
) -> Outcome {
    match shell_result(state, session, submission).await {
        Some(result) if result.exit_code == Some(0) => {
            let stdout = result.stdout_excerpt.trim();
            Outcome {
                status: FiringStatus::Ok,
                error: None,
                text: stdout.to_string(),
                deliver: !stdout.is_empty(),
            }
        }
        Some(result) => {
            let code = result
                .exit_code
                .map_or_else(|| "?".to_string(), |code| code.to_string());
            let stderr = tail_lines(&result.stderr_excerpt, 20);
            Outcome {
                status: FiringStatus::Error,
                error: None,
                text: format!("exit={code}\n{stderr}"),
                deliver: true,
            }
        }
        None => Outcome {
            status: FiringStatus::Error,
            error: None,
            text: "命令的执行结果读不出来（超时或结果不明时没有结构化结果），去原 Session 看细节（§10）"
                .to_string(),
            deliver: true,
        },
    }
}

/// `shell.command` entry 的 `data.output` 引用 → `output.json` 里的 `BashResult`。
async fn shell_result(
    state: &Arc<GatewayState>,
    session: &SessionId,
    submission: komo_kernel::durable::SubmissionId,
) -> Option<komo_runtime::tools::bash::BashResult> {
    let core = state.core().ok()?;
    let record = core.storage.submission(submission).await.ok()??;
    let komo_kernel::durable::SubmissionKind::Write {
        state: WriteState::Done { entry },
    } = record.kind
    else {
        return None;
    };
    let entry = core.storage.entry(entry).await.ok()??;
    let output: OutputRef =
        serde_json::from_value(entry.entry.data.as_ref()?.get("output")?.clone()).ok()?;
    let config = state.snapshot();
    let store = FileToolOutputStore::new(
        SessionPaths::new(&config.paths.sessions_dir, session),
        session.clone(),
    );
    let verified = store.open(&output).await.ok()?;
    serde_json::from_value(verified.body.result).ok()
}

fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// 收场：回写触发状态，按 `notify` 投 home chat。
async fn finish(state: &Arc<GatewayState>, watched: &Watched, outcome: Outcome) {
    let Outcome {
        status,
        error,
        text,
        deliver,
    } = outcome;
    tracing::Span::current().record("komo.cron.outcome", status.as_str());
    if status == FiringStatus::Error && watched.scheduled {
        tracing::event!(
            target: "komo::metrics",
            tracing::Level::DEBUG,
            {
            monotonic_counter.komo.cron.failures = 1u64,
            komo.cron.job = %watched.job,
            }
        );
    }
    settle(state, watched, status, error).await;
    if deliver {
        notify(
            state,
            watched,
            status,
            &watched.session,
            &watched.run,
            &text,
        )
        .await;
    }
}
