//! `/v1/runs`（§13.1）。

use axum::Json;
use axum::extract::{Path, State};
use komo_kernel::events::EventPayload;
use komo_kernel::fold::fold;
use komo_kernel::protocol::http::{
    CancelRunRequest, CancelRunResponse, RunDetail, RunSummary, ToolCallSummary,
};
use komo_kernel::types::ids::RunId;
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

    Ok(Json(RunDetail {
        summary: summary_of(&record),
        calls,
        final_message: view.and_then(|view| view.final_message.clone()),
        // 本次 Run 用到的记忆条目（§9.7）。MemoryManager 接进来之前它是空的。
        memories: Vec::new(),
    }))
}

/// `POST /v1/runs/{id}/cancel`
pub async fn cancel(
    State(api): State<Api>,
    Path(id): Path<String>,
    body: Option<Json<CancelRunRequest>>,
) -> ApiResult<Json<CancelRunResponse>> {
    let run = RunId::from_raw(id);
    let _ = body;
    let status = api.state.cancel_run(&run).await?;
    Ok(Json(CancelRunResponse { run, status }))
}

/// 一个 Run 的概览。
pub fn summary_of(record: &RunRecord) -> RunSummary {
    let created = created_at(record.run.as_str(), time::OffsetDateTime::UNIX_EPOCH);
    RunSummary {
        run: record.run.clone(),
        session: record.session.clone(),
        status: record.status,
        source: record.source.clone(),
        rounds: record.rounds,
        created_at: created,
        // 终态的时刻在 JSONL 的终态事件上；概览这一层只答"结没结束"。
        ended_at: None,
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
