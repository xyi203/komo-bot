//! Model context from a canonical pi path. Selection precedes payload I/O; execution
//! events remain untouched, including when a context edit removes a tool exchange.
use std::collections::{BTreeMap, BTreeSet};

use komo_kernel::events::{ContextCompacted, Event, EventPayload};
use komo_kernel::fold::{SurfaceMessage, fold};
use komo_kernel::projection::ProjectionContext;
use komo_kernel::session::{EntryKind, SessionTree};
use komo_kernel::session_events::TreeState;
use komo_kernel::types::ids::{RunId, Seq, ToolCallId};
use komo_kernel::types::tool::{ToolDefinition, ToolImage};
use komo_kernel::types::turn::{ReplayMessage, Role, ToolCallRequest, ToolResultForModel};
use serde_json::{Value, json};

use super::AgentContext;
use super::history::{self, DecayCandidate, Entry, ResolvedMessage};

pub fn enabled(events: &[Event]) -> bool {
    events.iter().any(|event| event.entry.is_some())
}

struct Message {
    value: Value,
    seq: Seq,
    source: bool,
    edited: bool,
}

/// Owned, selected execution facts. Only `sources` need payload/output reads.
/// API-created messages are translated directly and never require a source Run.
pub struct Selection {
    pub sources: Vec<SurfaceMessage>,
    pub decayed: BTreeSet<ToolCallId>,
    messages: Vec<Message>,
    calls: BTreeMap<String, ToolCallRequest>,
    pending: BTreeSet<ToolCallId>,
    pub accounting: Vec<Event>,
    pub image_refs: Vec<(Seq, Vec<komo_kernel::types::refs::ImageRef>)>,
    inherited_system: Vec<Value>,
    projections: BTreeMap<Seq, ProjectionContext>,
    stored: BTreeMap<Seq, history::StoredOutput>,
}

/// `run=None` selects the UI leaf (e.g. a manual summary); execution must pass its
/// Run so concurrent UI navigation cannot change recovery context. Delegate scopes
/// remain isolated, even for legacy parent/child Runs sharing one Session.
pub fn select(
    events: &[Event],
    run: Option<&RunId>,
    thread: Option<&[RunId]>,
) -> Result<Selection, String> {
    let execution = fold(events);
    let pending = run
        .filter(|run| {
            execution
                .runs
                .get(*run)
                .is_some_and(|view| view.status.is_unfinished())
        })
        .map(|run| execution.open_calls(run).into_iter().collect())
        .unwrap_or_default();
    let state = TreeState::from_events(events)?;
    let mut tree = state.tree()?;
    if let Some(run) = run {
        let head = state.head_for(run);
        // An accepted input whose start anchor is still missing is not visible.
        tree.branch(head.filter(|head| tree.get(head).is_some()))?;
    }
    let run_projections: BTreeMap<_, _> = events
        .iter()
        .filter_map(|event| {
            if let EventPayload::RunAccepted(body) = &event.payload {
                Some((event.run.as_ref()?, body.snapshot.as_ref()?.projection?))
            } else {
                None
            }
        })
        .collect();
    let projections = events
        .iter()
        .filter_map(|event| Some((event.seq, *run_projections.get(event.run.as_ref()?)?)))
        .collect();
    let owners: BTreeMap<_, _> = events
        .iter()
        .map(|event| {
            (
                event
                    .entry
                    .as_ref()
                    .map_or_else(|| event.event_id.to_string(), |entry| entry.id.clone()),
                event,
            )
        })
        .collect();
    if let Some(thread) = thread {
        // Keep tree edges for compaction/edit targets, but remove contributions
        // outside the historical delegate thread before projecting.
        let entries = tree
            .path(tree.leaf_id())?
            .into_iter()
            .cloned()
            .map(|mut entry| {
                if owners.get(&entry.id).is_some_and(|event| {
                    event.run.as_ref().is_some_and(|run| !thread.contains(run))
                }) {
                    entry.kind = EntryKind::Custom {
                        custom_type: "komo.outside-thread".into(),
                        data: None,
                    };
                }
                entry
            })
            .collect();
        tree = SessionTree::from_entries(entries)?;
    }
    let path = tree.path(tree.leaf_id())?;
    let path_ids: BTreeSet<_> = path.iter().map(|entry| entry.id.as_str()).collect();
    let accounting = events
        .iter()
        .filter(|event| {
            let id = event
                .entry
                .as_ref()
                .map_or(event.event_id.as_str(), |entry| entry.id.as_str());
            let on_path = path_ids.contains(id);
            let attached = matches!(event.payload, EventPayload::ContextCompacted(_))
                && state.entries().iter().any(|entry| {
                    entry.id == id
                        && entry
                            .parent_id
                            .as_deref()
                            .is_some_and(|id| path_ids.contains(id))
                });
            (on_path || attached)
                && thread
                    .is_none_or(|thread| event.run.as_ref().is_none_or(|run| thread.contains(run)))
        })
        .cloned()
        .collect();
    let mut calls = BTreeMap::new();
    let mut decayed = BTreeSet::new();
    for entry in &path {
        if let Some(event) = owners.get(&entry.id)
            && let EventPayload::MessageAssistant(body) = &event.payload
        {
            for call in &body.tool_calls {
                calls.insert(call.provider_call_id.clone(), call.clone());
            }
        }
    }
    for event in events {
        if let EventPayload::ContextCompacted(ContextCompacted::Decayed { calls, .. }) =
            &event.payload
        {
            let entry = state.entries().iter().find(|entry| {
                entry.id
                    == event
                        .entry
                        .as_ref()
                        .map_or(event.event_id.as_str(), |entry| entry.id.as_str())
            });
            if entry.is_some_and(|entry| {
                path_ids.contains(entry.id.as_str())
                    || entry
                        .parent_id
                        .as_deref()
                        .is_some_and(|id| path_ids.contains(id))
            }) && thread
                .is_none_or(|thread| event.run.as_ref().is_some_and(|run| thread.contains(run)))
            {
                decayed.extend(calls.iter().cloned());
            }
        }
    }
    // Old automatic compactions have no explicit system checkpoint. Reconstruct
    // it from this branch only; newly persisted checkpoints take precedence.
    let mut inherited_system = Vec::new();
    if let Some(index) = path
        .iter()
        .rposition(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
        && matches!(
            &path[index].kind,
            EntryKind::Compaction {
                system_message: None,
                ..
            }
        )
        && owners
            .get(&path[index].id)
            .is_some_and(|event| matches!(event.payload, EventPayload::ContextCompacted(_)))
    {
        for entry in &path[..index] {
            match &entry.kind {
                EntryKind::Message { message } if message["role"] == "system" => {
                    inherited_system.push(message.clone())
                }
                EntryKind::Compaction {
                    system_message: Some(system),
                    ..
                } => {
                    inherited_system.clear();
                    inherited_system.push(system.clone());
                }
                _ => {}
            }
        }
    }
    let selected = tree.context_entries()?;
    let edits: BTreeSet<_> = selected
        .iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::ContextEdit { target_id, .. } => Some(target_id.as_str()),
            _ => None,
        })
        .collect();
    let projection = tree.project()?;
    let mut projected = projection.messages.into_iter();
    let mut sources = Vec::new();
    let mut stored = BTreeMap::new();
    let mut image_refs = Vec::new();
    let mut messages = Vec::new();
    for (index, entry) in selected.iter().enumerate() {
        let omitted = selected
            .iter()
            .rev()
            .find_map(|edit| match &edit.kind {
                EntryKind::ContextEdit {
                    target_id,
                    replacement,
                } if target_id == &entry.id => Some(replacement.is_none()),
                _ => None,
            })
            .unwrap_or(false);
        if omitted {
            continue;
        }
        let count = match &entry.kind {
            EntryKind::Message { .. } | EntryKind::CustomMessage { .. } => 1,
            EntryKind::BranchSummary { summary, .. } => usize::from(!summary.is_empty()),
            EntryKind::Compaction { system_message, .. } if index == 0 => {
                1 + usize::from(system_message.is_some())
            }
            _ => 0,
        };
        let event = owners.get(&entry.id).copied();
        let seq = event.map_or(Seq::ZERO, |event| event.seq);
        let source = if count > 0 && !edits.contains(entry.id.as_str()) {
            event.and_then(execution_source)
        } else {
            None
        };
        let has_source = source.is_some();
        if let Some(source) = source {
            if let Some(event) = event
                && let EventPayload::ToolResult(result) = &event.payload
                && let Some(body) = &result.body
            {
                stored.insert(
                    seq,
                    history::StoredOutput {
                        preview: body.preview.clone(),
                        artifacts: body.artifacts.clone(),
                        images: body.images.clone(),
                        image_bytes: vec![],
                    },
                );
                image_refs.push((seq, body.images.clone()));
            }
            sources.push(source);
        } else if count > 0
            && let Some(event) = event
            && let Some(mut identity) = execution_source(event)
            && identity.role == Role::Assistant
        {
            identity.text = None;
            identity.text_ref = None;
            identity.provider_blocks = None;
            // Only identities are used for tool-result projection. Arguments are
            // never read for an edited source; the replacement owns their values.
            for call in &mut identity.tool_calls {
                call.arguments_ref = None;
            }
            sources.push(identity);
        }
        for _ in 0..count {
            let value = projected
                .next()
                .ok_or("session projection provenance mismatch")?;
            if !has_source && value["role"] == "toolResult" && !edits.contains(entry.id.as_str()) {
                if let Ok(images) = serde_json::from_value::<Vec<komo_kernel::types::refs::ImageRef>>(
                    value["details"]["komoImages"].clone(),
                ) {
                    image_refs.push((seq, images));
                }
            }
            messages.push(Message {
                value,
                seq,
                source: has_source,
                edited: edits.contains(entry.id.as_str()),
            });
        }
    }
    if projected.next().is_some() {
        return Err("session projection provenance overflow".into());
    }
    Ok(Selection {
        sources,
        decayed,
        messages,
        calls,
        pending,
        accounting,
        image_refs,
        inherited_system,
        projections,
        stored,
    })
}

fn execution_source(event: &Event) -> Option<SurfaceMessage> {
    match &event.payload {
        EventPayload::RunAccepted(_)
        | EventPayload::MessageUser(_)
        | EventPayload::MessageAssistant(_)
        | EventPayload::ToolResult(_) => {
            let mut message = fold([event]).messages.pop()?;
            if let EventPayload::ToolResult(result) = &event.payload
                && let Some(body) = &result.body
                && let Some(facts) = message.tool_results.first_mut()
            {
                facts.preview = body.preview.clone().or_else(|| facts.preview.clone());
            }
            Some(message)
        }
        EventPayload::ContextCompacted(ContextCompacted::Compacted {
            summary,
            summary_ref,
            ..
        }) => Some(SurfaceMessage {
            seq: event.seq,
            event_id: event.event_id.clone(),
            run: event.run.clone(),
            role: Role::User,
            text: summary.clone(),
            text_ref: summary_ref.clone(),
            tool_calls: vec![],
            tool_results: vec![],
            provider_blocks: None,
            absorbed_from: None,
        }),
        _ => None,
    }
}

impl Selection {
    /// Cache/debt accounting follows the whole branch, including summarized Runs.
    pub fn online_state(&self) -> komo_kernel::compaction::OnlineState {
        use komo_kernel::compaction::{CompactionDebt, OnlineState};
        let mut state = OnlineState::default();
        for event in &self.accounting {
            match &event.payload {
                EventPayload::MessageAssistant(body) => {
                    state.record_provider_request(body.input_tokens)
                }
                EventPayload::ToolResult(body)
                    if body.status == komo_kernel::types::refs::ToolResultStatus::Completed =>
                {
                    state.record_tool_work()
                }
                EventPayload::ContextCompacted(ContextCompacted::Compacted { debt, .. }) => {
                    state.record_compaction(*debt)
                }
                EventPayload::ContextCompacted(ContextCompacted::Decayed { debt, .. }) => {
                    state.record_decay(*debt)
                }
                EventPayload::ContextCompacted(ContextCompacted::Skipped { decision, .. }) => {
                    state.record_skipped(decision.as_ref().is_some_and(|decision| decision.compact))
                }
                _ if event
                    .entry
                    .as_ref()
                    .is_some_and(|entry| matches!(entry.kind, EntryKind::Compaction { .. })) =>
                {
                    state.record_compaction(CompactionDebt::default())
                }
                _ => {}
            }
        }
        state
    }

    pub fn entries(&self) -> Vec<Entry<'_>> {
        self.sources
            .iter()
            .map(|message| Entry {
                message,
                kind: history::EntryKind::Protocol,
                ending: None,
            })
            .collect()
    }

    /// Replace template history with the pi projection. Tool definitions are
    /// restored separately because AgentContext deliberately contains no schema.
    pub fn apply(
        &self,
        context: &mut AgentContext,
        mut resolved: Vec<ResolvedMessage<'_>>,
        projection: &ProjectionContext,
        tools: &mut Vec<ToolDefinition>,
    ) {
        for message in &mut resolved {
            if let Some(stored) = self.stored.get(&message.entry.message.seq) {
                for output in &mut message.outputs {
                    if output.is_none() {
                        *output = Some(stored.clone());
                    }
                }
            }
        }
        let (replay, _, candidates) = history::replay_with_projections(
            resolved,
            projection,
            &self.decayed,
            &self.projections,
        );
        let replay = replay
            .into_iter()
            .map(|mut message| {
                for result in &mut message.tool_results {
                    if let Some(call) = self
                        .calls
                        .values()
                        .find(|call| call.call_id == result.call_id)
                    {
                        result.provider_call_id.clone_from(&call.provider_call_id);
                    }
                }
                message
            })
            .collect::<Vec<_>>();
        let originals: BTreeMap<_, _> = replay
            .into_iter()
            .map(|message| (message.seq, message))
            .collect();
        let mut messages = Vec::new();
        let mut seen_system = false;
        for system in &self.inherited_system {
            if !seen_system && system.get("toolsAdded").is_some() {
                tools.clear();
            }
            apply_system(context, tools, system);
            seen_system = true;
        }
        for message in &self.messages {
            if message.value["role"] == "system" {
                if !seen_system && message.value.get("toolsAdded").is_some() {
                    tools.clear();
                }
                apply_system(context, tools, &message.value);
                seen_system = true;
                continue;
            }
            let original = message
                .source
                .then(|| originals.get(&message.seq))
                .flatten();
            let role = message.value["role"].as_str().unwrap_or_default();
            if role == "compactionSummary" {
                let text = original
                    .and_then(|message| message.text.clone())
                    .unwrap_or_else(|| string(&message.value["summary"]));
                messages.push(plain(
                    Role::User,
                    message.seq,
                    format!("{}\n\n{text}", history::COMPACTION_PREFIX),
                ));
            } else if let Some(original) = original {
                let mut original = original.clone();
                if original.role != Role::Tool
                    && original.text.is_none()
                    && !string(&message.value["content"]).is_empty()
                {
                    original.text = Some(string(&message.value["content"]));
                }
                messages.push(original);
            } else if let Some(mut converted) = convert(&message.value, message.seq, &self.calls) {
                if message.edited {
                    converted.provider_blocks = Some(json!({"komo_pi": message.value}));
                }
                messages.push(converted);
            }
        }
        // Frozen task prefixes still precede the task's own tree. A tree-mode
        // compaction can summarize the entire model context, including that prefix.
        let mut prefix = std::mem::take(&mut context.messages);
        // Frozen prefix seqs belong to the source Session, so they cannot become
        // first_kept references in this Session's execution ledger.
        for message in &mut prefix {
            message.seq = Seq::ZERO;
        }
        context.messages = prefix;
        context.messages.extend(messages);
        repair_pending(&mut context.messages, &self.pending);
        context.run_from = 0;
        context.decay_candidates = remap_candidates(candidates, &context.messages);
    }
}

fn remap_candidates(
    candidates: Vec<DecayCandidate>,
    messages: &[ReplayMessage],
) -> Vec<DecayCandidate> {
    candidates
        .into_iter()
        .filter_map(|mut candidate| {
            candidate.message = messages.iter().position(|message| {
                message
                    .tool_results
                    .iter()
                    .any(|result| result.call_id == candidate.call_id)
            })?;
            Some(candidate)
        })
        .collect()
}

fn plain(role: Role, seq: Seq, text: String) -> ReplayMessage {
    ReplayMessage {
        role,
        seq,
        text: Some(text),
        tool_calls: vec![],
        tool_results: vec![],
        provider_blocks: None,
    }
}

fn string(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    value
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn convert(
    value: &Value,
    seq: Seq,
    calls: &BTreeMap<String, ToolCallRequest>,
) -> Option<ReplayMessage> {
    let role = match value["role"].as_str()? {
        "assistant" => Role::Assistant,
        "toolResult" => Role::Tool,
        "user" | "custom" | "branchSummary" => Role::User,
        _ => return None,
    };
    let text = if value["role"] == "branchSummary" {
        string(&value["summary"])
    } else {
        string(&value["content"])
    };
    let mut message = plain(role, seq, text);
    // Retain pi arrays and opaque metadata without mistaking them for a provider's
    // native array (Anthropic/Responses use incompatible array schemas).
    message.provider_blocks = value
        .get("provider_blocks")
        .or_else(|| value.get("providerBlocks"))
        .filter(|value| !value.is_null())
        .cloned()
        .or_else(|| Some(json!({"komo_pi": value})));
    for block in value["content"].as_array().into_iter().flatten() {
        if role == Role::Assistant && block["type"] == "toolCall" {
            let id = block["id"].as_str().unwrap_or_default();
            let mut call = calls.get(id).cloned().unwrap_or_else(|| ToolCallRequest {
                call_id: internal_id(id),
                provider_call_id: id.into(),
                name: String::new(),
                arguments: Value::Null,
                arguments_ref: None,
            });
            call.name = string(&block["name"]);
            call.arguments = block["arguments"].clone();
            call.arguments_ref = None;
            message.tool_calls.push(call);
        }
    }
    if role == Role::Tool {
        let id = value["toolCallId"].as_str().unwrap_or_default();
        message.tool_results.push(ToolResultForModel {
            provider_call_id: id.into(),
            call_id: calls
                .get(id)
                .map_or_else(|| internal_id(id), |call| call.call_id.clone()),
            content: message.text.take().unwrap_or_default(),
            is_error: value["isError"].as_bool().unwrap_or(false),
            images: value["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(image)
                .collect(),
        });
    }
    Some(message)
}

fn internal_id(id: &str) -> ToolCallId {
    ToolCallId::from_raw(format!("pi-{id}"))
}

fn image(block: &Value) -> Option<ToolImage> {
    if block["type"] != "image" {
        return None;
    }
    let data = block["data"].as_str()?;
    let mut bytes = Vec::new();
    let mut bits = 0u32;
    let mut count = 0;
    for byte in data.bytes().filter(|byte| !byte.is_ascii_whitespace()) {
        if byte == b'=' {
            break;
        }
        let n = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(n);
        count += 6;
        if count >= 8 {
            count -= 8;
            bytes.push((bits >> count) as u8);
        }
    }
    Some(ToolImage {
        mime: block["mimeType"].as_str()?.into(),
        bytes,
    })
}

/// Restore the latest prompt and tool patches in projection order. Schema replay
/// never grants execution capability; the Gateway still supplies CallEnv.surface.
fn apply_system(context: &mut AgentContext, tools: &mut Vec<ToolDefinition>, value: &Value) {
    if let Some(content) = value.get("content") {
        let text = string(content);
        // pi normalizes a missing system content to ""; a schema-only patch
        // must not erase the prompt restored from the checkpoint.
        let patch = value.get("toolsAdded").is_some()
            || value.get("toolsRemoved").is_some()
            || value.get("sections").is_some();
        if !text.is_empty() || !patch {
            context.system_prompt = text;
        }
    }
    if let Some(prompt) = value.get("systemPrompt").and_then(Value::as_str) {
        context.system_prompt = prompt.into();
    }
    if let Some(prompt) = value["sections"]["prompt"].as_str() {
        context.system_prompt = prompt.into();
    }
    if let Some(full) = value["tools"].as_array() {
        *tools = full.iter().filter_map(tool).collect();
    }
    for removed in value["toolsRemoved"].as_array().into_iter().flatten() {
        let name = removed.as_str().or_else(|| removed["name"].as_str());
        tools.retain(|tool| Some(tool.name.as_str()) != name);
    }
    for added in value["toolsAdded"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(tool)
    {
        if let Some(slot) = tools.iter_mut().find(|tool| tool.name == added.name) {
            *slot = added;
        } else {
            tools.push(added);
        }
    }
}
fn tool(value: &Value) -> Option<ToolDefinition> {
    Some(ToolDefinition {
        name: value["name"].as_str()?.into(),
        description: value["description"].as_str().unwrap_or_default().into(),
        parameters: value
            .get("parameters")
            .or_else(|| value.get("inputSchema"))
            .cloned()
            .unwrap_or_else(|| json!({})),
        doc: None,
    })
}

/// Drop orphan results; settle half calls with an explicit model-only error before
/// the next non-tool message. Never turn an edited/cancelled call into execution.
#[cfg(test)]
fn repair(messages: &mut Vec<ReplayMessage>) {
    repair_pending(messages, &BTreeSet::new());
}
fn repair_pending(messages: &mut Vec<ReplayMessage>, live: &BTreeSet<ToolCallId>) {
    // A later adjudication can replace an uncertain result for the same call.
    // Keep its latest selected result within the assistant round, as fold does,
    // rather than consuming the call with the first partial result.
    let mut latest = BTreeMap::new();
    let mut round = 0;
    for (index, message) in messages.iter().enumerate() {
        if message.role == Role::Assistant {
            round += 1;
        }
        for (result_index, result) in message.tool_results.iter().enumerate() {
            latest.insert(
                (round, result.provider_call_id.clone()),
                (index, result_index),
            );
        }
    }
    round = 0;
    for (index, message) in messages.iter_mut().enumerate() {
        if message.role == Role::Assistant {
            round += 1;
        }
        let mut result_index = 0;
        message.tool_results.retain(|result| {
            let keep = latest.get(&(round, result.provider_call_id.clone()))
                == Some(&(index, result_index));
            result_index += 1;
            keep
        });
    }
    let mut out = Vec::new();
    let mut pending: Vec<ToolCallRequest> = Vec::new();
    for mut message in std::mem::take(messages) {
        if message.role != Role::Tool {
            settle(&mut out, &mut pending);
        }
        if message.role == Role::Tool {
            message.tool_results.retain(|result| {
                pending
                    .iter()
                    .any(|call| call.provider_call_id == result.provider_call_id)
            });
            for result in &message.tool_results {
                pending.retain(|call| call.provider_call_id != result.provider_call_id);
            }
            if message.tool_results.is_empty() {
                continue;
            }
        }
        pending.extend(message.tool_calls.iter().cloned());
        out.push(message);
    }
    pending.retain(|call| !live.contains(&call.call_id));
    settle(&mut out, &mut pending);
    *messages = out;
}
fn settle(out: &mut Vec<ReplayMessage>, pending: &mut Vec<ToolCallRequest>) {
    if pending.is_empty() {
        return;
    }
    let seq = out.last().map_or(Seq::ZERO, |message| message.seq);
    out.push(ReplayMessage { role: Role::Tool, seq, text: None, tool_calls: vec![], provider_blocks: None,
        tool_results: pending.drain(..).map(|call| ToolResultForModel { provider_call_id: call.provider_call_id, call_id: call.call_id, content: "Tool result unavailable in the selected context (cancelled, interrupted, or edited).".into(), is_error: true, images: vec![] }).collect() });
}

#[cfg(test)]
mod tests;
