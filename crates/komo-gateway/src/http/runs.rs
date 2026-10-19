//! `/v1/runs`（§13.1）。

use axum::Json;
use axum::extract::{Path, State};
use komo_kernel::events::{ContextCompacted, Event, EventPayload};
use komo_kernel::fold::fold;
use komo_kernel::protocol::http::{
    CancelRunRequest, CancelRunResponse, CompactionSummary, RunDetail, RunSummary, TokenTotals,
    ToolCallSummary,
};
use komo_kernel::types::ids::RunId;
use komo_kernel::types::memory::RecallDiagnostics;
use komo_kernel::types::status::RunState;
use komo_store::repos::runs::RunRecord;

use super::error::{ApiFailure, ApiResult};
use super::{Api, created_at, events_of};

/// `GET /v1/runs/{id}`：执行过程、工具结果与产物。
pub async fn detail(State(api): State<Api>, Path(id): Path<String>) -> ApiResult<Json<RunDetail>> {
    let run = RunId::from_raw(id);
    let record = komo_store::repos::runs::get(&api.state.db, &run)
        .await?
        .ok_or_else(|| ApiFailure::not_found(format!("run {run}")))?;

    let events = events_of(&api, &record.session).await?;
    let surface = fold(&events);
    let view = surface.runs.get(&run);

    let mut calls = Vec::new();
    if let Some(view) = view {
        for call_id in &view.calls {
            let Some(call) = surface.calls.get(call_id) else {
                continue;
            };
            calls.push(ToolCallSummary {
                call: call_id.clone(),
                tool: tool_name(&events, call_id).unwrap_or_else(|| "（未知工具）".into()),
                state: call.state,
                attempts: call.attempts,
                plan_hash: call.plan_hash.clone(),
                output: call.output.clone(),
                preview: preview(&surface, call_id),
            });
        }
    }

    let mut summary = summary_of(&record);
    // 库里没记（老行）时取终态事件的时刻。
    summary.ended_at = summary.ended_at.or_else(|| terminal_at(&events, &run));
    let duration_ms = summary.ended_at.map(|ended| {
        (ended - summary.created_at)
            .whole_milliseconds()
            .max(0)
            .try_into()
            .unwrap_or(u64::MAX)
    });
    let (memories, recall) = injection_of(&events, &run);

    Ok(Json(RunDetail {
        summary,
        calls,
        final_message: view.and_then(|view| view.final_message.clone()),
        memories,
        recall,
        model: model_of(&events, &run),
        duration_ms,
        failure: record
            .last_error
            .clone()
            .or_else(|| failure_of(&events, &run)),
        tokens: token_totals(&events, &run),
        compactions: compactions_of(&events, &run),
    }))
}

fn of_run<'a>(events: &'a [Event], run: &'a RunId) -> impl Iterator<Item = &'a Event> {
    events
        .iter()
        .filter(move |event| event.run.as_ref() == Some(run))
}

fn terminal_at(events: &[Event], run: &RunId) -> Option<time::OffsetDateTime> {
    of_run(events, run).find_map(|event| match event.payload {
        EventPayload::RunCompleted(_)
        | EventPayload::RunFailed(_)
        | EventPayload::RunCancelled(_)
        | EventPayload::RunAbandoned(_) => Some(event.ts),
        _ => None,
    })
}

fn failure_of(events: &[Event], run: &RunId) -> Option<String> {
    of_run(events, run).find_map(|event| match &event.payload {
        EventPayload::RunFailed(body) => Some(body.reason.clone()),
        _ => None,
    })
}

fn model_of(events: &[Event], run: &RunId) -> Option<String> {
    of_run(events, run).find_map(|event| match &event.payload {
        EventPayload::RunAccepted(body) => body
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.model.model.clone())
            .or_else(|| body.model.clone()),
        _ => None,
    })
}

/// 各轮用量求和。某一项从没人报过就保持 `None`（未知不是零，§8.5）。
fn token_totals(events: &[Event], run: &RunId) -> Option<TokenTotals> {
    fn add(total: &mut Option<u64>, value: Option<u64>) {
        if let Some(value) = value {
            *total = Some(total.unwrap_or(0).saturating_add(value));
        }
    }
    let mut totals = TokenTotals::default();
    for event in of_run(events, run) {
        if let EventPayload::MessageAssistant(body) = &event.payload {
            add(&mut totals.input, body.input_tokens);
            add(&mut totals.output, body.output_tokens);
            add(&mut totals.cache_read, body.cache_read_tokens);
            add(&mut totals.cache_write, body.cache_write_tokens);
        }
    }
    (totals != TokenTotals::default()).then_some(totals)
}

fn compactions_of(events: &[Event], run: &RunId) -> Vec<CompactionSummary> {
    fn reason(decision: &komo_kernel::compaction::CompactionDecision) -> Option<String> {
        serde_json::to_value(decision.reason)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
    }
    of_run(events, run)
        .filter_map(|event| match &event.payload {
            EventPayload::ContextCompacted(ContextCompacted::Compacted { decision, .. }) => {
                Some(CompactionSummary {
                    outcome: "compacted".into(),
                    archive_tokens: Some(decision.archive_tokens),
                    post_compaction_tokens: Some(decision.post_compaction_tokens),
                    decayed_calls: None,
                    reason: reason(decision),
                })
            }
            EventPayload::ContextCompacted(ContextCompacted::Decayed {
                calls, decision, ..
            }) => Some(CompactionSummary {
                outcome: "decayed".into(),
                archive_tokens: Some(decision.archive_tokens),
                post_compaction_tokens: Some(decision.post_compaction_tokens),
                decayed_calls: Some(calls.len() as u32),
                reason: reason(decision),
            }),
            EventPayload::ContextCompacted(ContextCompacted::Skipped {
                reason: why,
                decision,
            }) => Some(CompactionSummary {
                outcome: "skipped".into(),
                archive_tokens: decision.as_ref().map(|d| d.archive_tokens),
                post_compaction_tokens: decision.as_ref().map(|d| d.post_compaction_tokens),
                decayed_calls: None,
                reason: Some(why.clone()),
            }),
            _ => None,
        })
        .collect()
}

/// 这条 Run 的系统提示里带的记忆（§9.7）。
///
/// 同一段对话里注入段只在换块时才记一条 `memory.injected`，后面的 Run 沿用它而不再记；
/// 所以取**这条 Run 结束前最近的一条**：是自己记的就带上召回诊断，是更早的 Run 记的就
/// 标"沿用"。
fn injection_of(
    events: &[Event],
    run: &RunId,
) -> (
    Vec<komo_kernel::types::turn::MemoryUse>,
    Option<RecallDiagnostics>,
) {
    let Some(last) = of_run(events, run).map(|event| event.seq).max() else {
        return (Vec::new(), None);
    };
    let Some((event, body)) = komo_kernel::events::injection_at(events, last) else {
        return (Vec::new(), None);
    };
    let recall = if event.run.as_ref() == Some(run) {
        body.recall.clone()
    } else if body.uses.is_empty() {
        None
    } else {
        Some(RecallDiagnostics {
            reused_pin: true,
            ..RecallDiagnostics::default()
        })
    };
    (body.uses.clone(), recall)
}

/// `POST /v1/runs/{id}/cancel`
pub async fn cancel(
    State(api): State<Api>,
    Path(id): Path<String>,
    body: Option<Json<CancelRunRequest>>,
) -> ApiResult<Json<CancelRunResponse>> {
    let run = RunId::from_raw(id);
    let _ = body;
    let state = api.state.cancel_run(&run).await?;
    Ok(Json(CancelRunResponse { run, state }))
}

/// 一个 Run 的概览。
///
/// `wait` 是**唯一**看得到 `Dependency` 的地方之一（另一处是 `SessionSummary` 的
/// `current_wait`）：等前一条 Run 的那一类不进 §7.5 的清单——它不是在等人（§7.5 第 2 条），
/// 但它仍然挡着同 Session 后面的 Run，所以它必须在这一格说得出来。
pub fn summary_of(record: &RunRecord) -> RunSummary {
    let created = created_at(record.run.as_str(), time::OffsetDateTime::UNIX_EPOCH);
    RunSummary {
        run: record.run.clone(),
        session: record.session.clone(),
        state: record.state,
        // 只在 `waiting` 时有值（§8.4）：`queued` 的 Run "一定说得出有活干"。
        wait: (record.state == RunState::Waiting)
            .then(|| record.wait.clone())
            .flatten(),
        source: record.source.clone(),
        rounds: record.rounds,
        created_at: created,
        ended_at: record.ended_at,
    }
}

fn tool_name(
    events: &[komo_kernel::events::Event],
    call: &komo_kernel::types::ids::ToolCallId,
) -> Option<String> {
    events.iter().rev().find_map(|event| match &event.payload {
        EventPayload::MessageAssistant(body) => body
            .tool_calls
            .iter()
            .find(|candidate| &candidate.call_id == call)
            .map(|candidate| candidate.name.clone()),
        _ => None,
    })
}

fn preview(
    surface: &komo_kernel::fold::Surface,
    call: &komo_kernel::types::ids::ToolCallId,
) -> Option<String> {
    surface.messages.iter().rev().find_map(|message| {
        message
            .tool_results
            .iter()
            .find(|result| &result.call == call)
            .and_then(|result| result.preview.clone())
    })
}
