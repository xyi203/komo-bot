//! pi JSONL entries carry komo execution metadata without a second message body.
use serde_json::{Value, json};

use super::{Event, RawEvent};
use crate::session::SessionEntry;

pub(super) fn encode(event: &Event, entry: &SessionEntry) -> Result<Value, serde_json::Error> {
    let mut value = serde_json::to_value(entry)?;
    let mut execution = serde_json::to_value(event.to_raw()?)?;
    let data = &mut execution["data"];
    match event.type_name() {
        "run.accepted" | "message.user" | "message.assistant" => {
            if let Some(data) = data.as_object_mut() {
                let present = data.contains_key("text");
                data.insert("text_present".into(), json!(present));
                data.remove("text");
                data.remove("provider_blocks");
                if let Some(calls) = data.get_mut("tool_calls").and_then(Value::as_array_mut) {
                    for call in calls {
                        if let Some(call) = call.as_object_mut() {
                            call.remove("arguments");
                        }
                    }
                }
            }
        }
        "run.completed" => {
            if let Some(data) = data.as_object_mut() {
                data.remove("final_message");
            }
        }
        "context.compacted" if value["type"] == "compaction" => {
            if let Some(data) = data.as_object_mut() {
                data.remove("summary");
            }
        }
        _ => {}
    }
    // An old komo must fail on this record, rather than replay sibling branches linearly.
    value["v"] = json!(2);
    value["komo"] = execution;
    Ok(value)
}

pub(super) fn decode(value: Value) -> Result<Event, String> {
    if value["v"] != 2 {
        return Err("unsupported session tree record version".into());
    }
    let entry: SessionEntry = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    let mut execution = value["komo"].clone();
    let kind = execution["type"].as_str().unwrap_or_default().to_string();
    let data = &mut execution["data"];
    match kind.as_str() {
        "run.accepted" | "message.user" | "message.assistant" => {
            let present = data
                .get("text_present")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if data.get("text_ref").is_none() && present {
                data["text"] = json!(content_text(&value["message"]["content"]));
            }
            if kind == "message.assistant" && !value["message"]["providerBlocks"].is_null() {
                data["provider_blocks"] = value["message"]["providerBlocks"].clone();
            }
            if let Some(calls) = data.get_mut("tool_calls").and_then(Value::as_array_mut) {
                for call in calls {
                    let id = call["provider_call_id"].as_str().unwrap_or_default();
                    let arguments = value["message"]["content"]
                        .as_array()
                        .and_then(|blocks| {
                            blocks
                                .iter()
                                .find(|block| block["type"] == "toolCall" && block["id"] == id)
                        })
                        .map(|block| block["arguments"].clone())
                        .unwrap_or(Value::Null);
                    call["arguments"] = arguments;
                }
            }
        }
        "run.completed" if value["type"] == "message" => {
            data["final_message"] = json!(content_text(&value["message"]["content"]));
        }
        "context.compacted" if value["type"] == "compaction" => {
            data["summary"] = value["summary"].clone();
        }
        _ => {}
    }
    let raw: RawEvent = serde_json::from_value(execution).map_err(|e| e.to_string())?;
    let mut event = Event::from_raw(raw).map_err(|e| e.to_string())?;
    event.entry = Some(Box::new(entry));
    Ok(event)
}

pub fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}
