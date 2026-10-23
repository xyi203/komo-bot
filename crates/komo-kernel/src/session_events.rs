//! Canonical session entries and execution anchors. Pending inputs own their text once;
//! their anchor is linked when the input actually starts or is absorbed as steering.
use crate::events::{ContextCompacted, Event, EventPayload};
use crate::session::{SessionEntry, SessionTree};
use crate::types::ids::RunId;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TreeState {
    entries: Vec<SessionEntry>,
    ids: BTreeSet<String>,
    pub leaf: Option<String>,
    heads: BTreeMap<RunId, String>,
    inputs: BTreeMap<RunId, String>,
    owners: BTreeMap<RunId, RunId>,
    calls: BTreeMap<String, (String, String)>,
    models: BTreeMap<RunId, (String, String)>,
    projections: BTreeMap<RunId, crate::projection::ProjectionContext>,
    seq_ids: BTreeMap<u64, String>,
}

fn anchor(run: &RunId) -> String {
    format!("input-{run}")
}
fn make(id: String, parent: Option<String>, event: &Event, mut fields: Value) -> SessionEntry {
    fields["id"] = json!(id);
    fields["parentId"] = json!(parent);
    fields["timestamp"] = json!(
        event
            .ts
            .format(&time::format_description::well_known::Rfc3339)
            .expect("valid timestamp")
    );
    serde_json::from_value(fields).expect("session entry fields")
}

impl TreeState {
    pub fn from_events(events: &[Event]) -> Result<Self, String> {
        let mut state = Self::default();
        for event in events {
            let entry = state.entry_for(event);
            state.apply(event, entry)?;
        }
        Ok(state)
    }

    pub fn hydrate_final(&self, event: &mut Event) {
        if let EventPayload::RunCompleted(body) = &mut event.payload
            && body.final_message.is_none() && body.final_message_ref.is_none()
            && let Some(entry) = event.entry.as_deref().and_then(|own| {
                if matches!(&own.kind,crate::session::EntryKind::Message{message} if message["role"]=="assistant") {Some(own)} else {
                    own.parent_id.as_ref().and_then(|parent|self.entries.iter().find(|entry|&entry.id==parent))
                }
            })
            && let crate::session::EntryKind::Message {message} = &entry.kind
            && message["role"] == "assistant"
        {
            body.final_message = Some(match &message["content"] {
                Value::String(text)=>text.clone(),
                Value::Array(blocks)=>blocks.iter().filter(|b|b["type"]=="text").filter_map(|b|b["text"].as_str()).collect::<Vec<_>>().join("\n"),
                _=>String::new(),
            });
        }
    }

    pub fn head_for(&self, run: &RunId) -> Option<&str> {
        self.heads.get(run).map(String::as_str)
    }
    pub fn entry_at_seq(&self, seq: u64) -> Option<&SessionEntry> {
        let id = self.seq_ids.get(&seq)?;
        self.entries.iter().find(|entry| &entry.id == id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    pub fn entry_for(&self, event: &Event) -> SessionEntry {
        if let Some(entry) = &event.entry {
            return entry.as_ref().clone();
        }
        let id = event.event_id.to_string();
        let owner = event.run.as_ref().map(|r| self.owners.get(r).unwrap_or(r));
        let parent = owner
            .and_then(|r| self.heads.get(r))
            .cloned()
            .or_else(|| self.leaf.clone());
        let mut fields = json!({"type":"custom","customType":"komo.execution"});
        let mut parent = parent;
        let mut entry_id = id;
        match &event.payload {
            EventPayload::RunAccepted(body) => {
                if let Some(run) = &event.run {
                    parent = Some(anchor(run));
                }
                fields = json!({"type":"message","message":{"role":"user","content":body.text.clone().unwrap_or_default(),"timestamp":event.ts.unix_timestamp_nanos()/1_000_000}});
            }
            EventPayload::RunStarted(_) => {
                if let Some(run) = &event.run
                    && !self.ids.contains(&anchor(run))
                {
                    entry_id = anchor(run);
                    parent = self.leaf.clone();
                    let (provider, model) = self.models.get(run).cloned().unwrap_or_default();
                    fields = json!({"type":"model_change","provider":provider,"modelId":model});
                }
            }
            EventPayload::RunAbsorbed(body) => {
                if let Some(run) = &event.run {
                    entry_id = anchor(run);
                }
                parent = self
                    .heads
                    .get(&body.into)
                    .cloned()
                    .or_else(|| self.leaf.clone());
            }
            EventPayload::RunCompleted(body) if body.final_message.is_some() => {
                let text = body.final_message.as_deref().unwrap_or_default();
                let existing = parent
                    .as_ref()
                    .and_then(|id| self.entries.iter().find(|entry| &entry.id == id))
                    .and_then(|entry| match &entry.kind {
                        crate::session::EntryKind::Message { message }
                            if message["role"] == "assistant" =>
                        {
                            Some(message)
                        }
                        _ => None,
                    });
                let same = existing.is_some_and(|message| {
                    message["content"].as_array().is_some_and(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| b["type"] == "text")
                            .filter_map(|b| b["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                            == text
                    })
                });
                if !same {
                    fields = json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":text}],"provider":"","model":"","api":"komo","stopReason":"stop","timestamp":event.ts.unix_timestamp_nanos()/1_000_000}});
                }
            }
            EventPayload::MessageUser(body) => {
                fields = json!({"type":"message","message":{"role":"user","content":body.text.clone().unwrap_or_default(),"timestamp":event.ts.unix_timestamp_nanos()/1_000_000}});
            }
            EventPayload::MessageAssistant(body) => {
                let mut content = Vec::new();
                if let Some(text) = &body.text {
                    content.push(json!({"type":"text","text":text}));
                }
                for call in &body.tool_calls {
                    content.push(json!({"type":"toolCall","id":call.provider_call_id,"name":call.name,"arguments":call.arguments}));
                }
                let (provider, model) = event
                    .run
                    .as_ref()
                    .and_then(|r| self.models.get(r))
                    .cloned()
                    .unwrap_or_default();
                fields = json!({"type":"message","message":{"role":"assistant","content":content,"provider":provider,"model":model,"api":"komo","stopReason":if body.tool_calls.is_empty(){"stop"}else{"toolUse"},"timestamp":event.ts.unix_timestamp_nanos()/1_000_000,"usage":{"input":body.input_tokens.unwrap_or(0),"output":body.output_tokens.unwrap_or(0),"cacheRead":body.cache_read_tokens.unwrap_or(0),"cacheWrite":body.cache_write_tokens.unwrap_or(0),"totalTokens":body.input_tokens.unwrap_or(0)+body.output_tokens.unwrap_or(0),"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}}});
                fields["message"]["providerBlocks"] = json!(body.provider_blocks);
            }
            EventPayload::ToolResult(body) => {
                let (provider_id, name) = self
                    .calls
                    .get(body.call_id.as_str())
                    .cloned()
                    .unwrap_or_else(|| (body.call_id.to_string(), String::new()));
                let context = event
                    .run
                    .as_ref()
                    .and_then(|run| self.projections.get(run))
                    .copied()
                    .unwrap_or(crate::projection::ProjectionContext {
                        model_result_bytes: crate::projection::DEFAULT_MODEL_RESULT_BYTES,
                        decay: None,
                    });
                let body_facts = body.body.as_deref();
                let text = crate::projection::project(
                    &crate::projection::ToolResultFacts {
                        tool: &name,
                        status: body.status,
                        elapsed_ms: body.elapsed_ms,
                        text: body_facts
                            .and_then(|b| b.preview.as_deref())
                            .or(body.preview.as_deref()),
                        output: &body.output_ref,
                        stdout: body.stdout.as_ref(),
                        stderr: body.stderr.as_ref(),
                        artifacts: body_facts
                            .map(|b| b.artifacts.as_slice())
                            .unwrap_or_default(),
                        images: body_facts.map(|b| b.images.as_slice()).unwrap_or_default(),
                    },
                    &context,
                );
                fields = json!({"type":"message","message":{"role":"toolResult","toolCallId":provider_id,"toolName":name,"content":[{"type":"text","text":text}],"details":{"komoImages":body_facts.map(|b|&b.images),"komoOutput":body.output_ref},"isError":body.status!=crate::types::refs::ToolResultStatus::Completed,"timestamp":event.ts.unix_timestamp_nanos()/1_000_000}});
            }
            EventPayload::ContextCompacted(ContextCompacted::Compacted {
                first_kept,
                summary,
                ..
            }) => {
                fields = json!({"type":"compaction","summary":summary.clone().unwrap_or_default(),"firstKeptEntryId":self.seq_ids.get(&first_kept.0).cloned().unwrap_or_else(||event.event_id.to_string()),"tokensBefore":0});
            }
            EventPayload::ConversationBoundary(_) => {
                parent = None;
                fields = json!({"type":"custom","customType":"komo.boundary"});
            }
            EventPayload::ConfigChanged(body) if body.model.is_some() => {
                fields = json!({"type":"model_change","provider":"","modelId":body.model});
            }
            _ => {}
        }
        make(entry_id, parent, event, fields)
    }

    pub fn apply(&mut self, event: &Event, entry: SessionEntry) -> Result<(), String> {
        if !self.ids.insert(entry.id.clone()) {
            return Err(format!("duplicate session entry {}", entry.id));
        }
        self.seq_ids.insert(event.seq.0, entry.id.clone());
        let value = serde_json::to_value(&entry).map_err(|e| e.to_string())?;
        let mut advance = matches!(
            value["type"].as_str(),
            Some(
                "message"
                    | "compaction"
                    | "branch_summary"
                    | "context_edit"
                    | "model_change"
                    | "thinking_level_change"
                    | "custom_message"
                    | "label"
                    | "session_info"
                    | "usage"
            )
        );
        match &event.payload {
            EventPayload::RunAccepted(body) => {
                if let Some(run) = &event.run {
                    self.inputs.insert(run.clone(), entry.id.clone());
                    self.heads.insert(run.clone(), entry.id.clone());
                    let model = body
                        .snapshot
                        .as_ref()
                        .map(|s| (s.model.provider.clone(), s.model.model.clone()))
                        .unwrap_or_else(|| (String::new(), body.model.clone().unwrap_or_default()));
                    self.models.insert(run.clone(), model);
                    if let Some(context) = body.snapshot.as_ref().and_then(|s| s.projection) {
                        self.projections.insert(run.clone(), context);
                    }
                }
                advance = false;
            }
            EventPayload::RunStarted(_) => {
                if let Some(run) = &event.run
                    && let Some(head) = self.heads.get(run)
                {
                    self.leaf = Some(head.clone());
                }
                advance = false;
            }
            EventPayload::RunAbsorbed(body) => {
                if let Some(run) = &event.run {
                    self.owners.insert(run.clone(), body.into.clone());
                    if let Some(input) = self.inputs.get(run) {
                        self.heads.insert(body.into.clone(), input.clone());
                        self.leaf = Some(input.clone());
                    }
                }
                advance = false;
            }
            EventPayload::ConversationBoundary(_) => {
                self.leaf = Some(entry.id.clone());
                advance = false;
            }
            EventPayload::MessageAssistant(body) => {
                for call in &body.tool_calls {
                    self.calls.insert(
                        call.call_id.to_string(),
                        (call.provider_call_id.clone(), call.name.clone()),
                    );
                }
            }
            _ => {}
        }
        if advance {
            if let Some(run) = &event.run {
                self.heads.insert(
                    self.owners.get(run).unwrap_or(run).clone(),
                    entry.id.clone(),
                );
            }
            self.leaf = Some(entry.id.clone());
        }
        self.entries.push(entry);
        Ok(())
    }

    /// Pending inputs are durable but not yet members of a finalized conversation path.
    pub fn tree(&self) -> Result<SessionTree, String> {
        let mut remaining = self.entries.iter().collect::<Vec<_>>();
        let mut sorted = Vec::new();
        let mut known = BTreeSet::new();
        loop {
            let before = remaining.len();
            remaining.retain(|entry| {
                if entry.parent_id.as_ref().is_none_or(|id| known.contains(id)) {
                    known.insert(entry.id.clone());
                    sorted.push((*entry).clone());
                    false
                } else {
                    true
                }
            });
            if remaining.len() == before {
                break;
            }
        }
        let mut tree = SessionTree::from_entries(sorted)?;
        tree.branch(self.leaf.as_deref())?;
        Ok(tree)
    }

    pub fn branch(&mut self, target: Option<&str>) -> Result<(), String> {
        let mut tree = self.tree()?;
        tree.branch(target)?;
        self.leaf = target.map(str::to_string);
        Ok(())
    }
}
