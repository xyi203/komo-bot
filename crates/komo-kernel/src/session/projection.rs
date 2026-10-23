use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{EntryKind, SessionEntry, SessionTree};

/// pi's AgentMessages, before provider-specific conversion/repair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionProjection {
    pub messages: Vec<Value>,
    pub model: Option<(String, String)>,
    pub thinking_level: String,
}

impl SessionTree {
    /// Latest compaction, its retained non-system range, then later entries.
    /// A self-referencing first-kept ID (or one absent from the earlier path)
    /// retains none. Older compactions may remain as raw selected entries.
    pub fn context_entries(&self) -> Result<Vec<&SessionEntry>, String> {
        let path = self.path(self.leaf_id())?;
        let Some(index) = path
            .iter()
            .rposition(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
        else {
            return Ok(path);
        };
        let EntryKind::Compaction {
            first_kept_entry_id,
            ..
        } = &path[index].kind
        else {
            unreachable!()
        };
        let mut selected = vec![path[index]];
        if let Some(first) = path[..index]
            .iter()
            .position(|entry| &entry.id == first_kept_entry_id)
        {
            selected.extend(path[first..index].iter().copied().filter(|entry| {
                !matches!(&entry.kind, EntryKind::Message { message } if message["role"] == "system")
            }));
        }
        selected.extend_from_slice(&path[index + 1..]);
        Ok(selected)
    }

    pub fn project(&self) -> Result<SessionProjection, String> {
        let mut model = None;
        let mut thinking_level = "off".to_owned();
        // Settings come from the whole path, including summarized entries.
        for entry in self.path(self.leaf_id())? {
            match &entry.kind {
                EntryKind::ThinkingLevelChange {
                    thinking_level: level,
                } => {
                    thinking_level.clone_from(level);
                }
                EntryKind::ModelChange { provider, model_id } => {
                    model = Some((provider.clone(), model_id.clone()));
                }
                EntryKind::Message { message } if message["role"] == "assistant" => {
                    if let (Some(provider), Some(id)) =
                        (message["provider"].as_str(), message["model"].as_str())
                    {
                        model = Some((provider.to_owned(), id.to_owned()));
                    }
                }
                _ => {}
            }
        }
        let selected = self.context_entries()?;
        let mut edits = HashMap::new();
        for entry in &selected {
            if let EntryKind::ContextEdit {
                target_id,
                replacement,
            } = &entry.kind
            {
                edits.insert(target_id.as_str(), replacement.as_ref());
            }
        }
        let mut messages = Vec::new();
        for (index, entry) in selected.into_iter().enumerate() {
            // A retained older compaction contributes neither summary nor system.
            if index > 0 && matches!(entry.kind, EntryKind::Compaction { .. }) {
                continue;
            }
            let contribution = entry_messages(entry);
            match edits.get(entry.id.as_str()) {
                Some(None) => {}
                Some(Some(replacement)) => {
                    for mut message in contribution {
                        if matches!(
                            message["role"].as_str(),
                            Some("user" | "assistant" | "toolResult" | "custom")
                        ) {
                            let content = replacement["content"].clone();
                            message["content"] = if content.is_string()
                                && matches!(
                                    message["role"].as_str(),
                                    Some("assistant" | "toolResult")
                                ) {
                                json!([{ "type": "text", "text": content }])
                            } else {
                                content
                            };
                        }
                        messages.push(message);
                    }
                }
                None => messages.extend(contribution),
            }
        }
        Ok(SessionProjection {
            messages,
            model,
            thinking_level,
        })
    }
}

fn entry_messages(entry: &SessionEntry) -> Vec<Value> {
    match &entry.kind {
        EntryKind::Message { message } => {
            let mut message = message.clone();
            if message["content"].is_null() {
                match message["role"].as_str() {
                    Some("system") => message["content"] = json!(""),
                    Some("user" | "assistant" | "toolResult") => message["content"] = json!([]),
                    _ => {}
                }
            }
            vec![message]
        }
        EntryKind::CustomMessage {
            custom_type,
            content,
            display,
            details,
        } => {
            let mut message = json!({
                "role": "custom", "customType": custom_type,
                "content": if content.is_null() { json!([]) } else { content.clone() },
                "display": display, "timestamp": timestamp(&entry.timestamp),
            });
            if let Some(details) = details {
                message["details"] = details.clone();
            }
            vec![message]
        }
        EntryKind::BranchSummary {
            from_id, summary, ..
        } if !summary.is_empty() => {
            vec![json!({
                "role": "branchSummary", "summary": summary, "fromId": from_id,
                "timestamp": timestamp(&entry.timestamp),
            })]
        }
        EntryKind::Compaction {
            summary,
            tokens_before,
            system_message,
            ..
        } => {
            let mut messages = Vec::new();
            if let Some(system) = system_message {
                messages.push(system.clone());
            }
            messages.push(json!({
                "role": "compactionSummary", "summary": summary, "tokensBefore": tokens_before,
                "timestamp": timestamp(&entry.timestamp),
            }));
            messages
        }
        _ => Vec::new(),
    }
}

/// Session entries use ISO strings; generated AgentMessages use Unix millis.
/// Invalid dates match JSON.stringify(new Date(invalid).getTime()): null.
fn timestamp(value: &str) -> Value {
    OffsetDateTime::parse(value, &Rfc3339)
        .ok()
        .and_then(|time| i64::try_from(time.unix_timestamp_nanos().div_euclid(1_000_000)).ok())
        .map_or(Value::Null, |millis| json!(millis))
}
