//! codemode 脚本里子调用的展示视图（`docs/codemode-bash.md` §8；照 pi 的
//! `CodemodeNestedCall`：子调用不是模型发起的调用，所以不单独成行，画在外层那一行里）。
//!
//! 两个来源、一个形状：
//!
//! - **跑完了**：外层结果 `output.json` 的 `result.calls`（runtime `executor::codemode::NestedCall`）。
//!   Gateway 读出来经 [`display_result`] 收窄后放进 `tool.result` 的 `body.result`，补读与直播走同一个
//!   函数，所以重连前后逐字节相同；
//! - **还在跑**：`komo.live.nestedTools` 的槽位加 `komo.nested` 成员，Gateway 合成
//!   [`crate::protocol::SseEvent::NestedCalls`]，只在 SSE 上，跑完后被上面那份取代。
//!
//! 状态统一由 [`state_of`] 判，参数统一由 [`bounded_args`] 收窄——两条路径不会各说各话。
//! 这里的东西**只给界面**：不进模型 transcript，不是结果的权威（权威是 `output.json` 本身）。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::types::refs::ToolResultStatus;
use crate::types::status::ToolCallState;

/// 一次外层调用最多列多少个子调用（与 runtime 的 `MAX_CALLS` 同值）。
pub const MAX_VIEW_CALLS: usize = 256;
/// 子调用的失败原因最多留多少字节（pi `NESTED_CALL_LIMITS.maxErrorChars` 是 500 字符）。
pub const ERROR_VIEW_BYTES: usize = 500;
/// 参数里一段字符串最多留多少字符。
const ARG_STRING_CHARS: usize = 200;
/// 参数里一个数组 / 对象最多留多少项。
const ARG_ITEMS: usize = 16;
/// 参数最多留几层。
const ARG_DEPTH: usize = 3;

/// 一个子调用在界面上的样子。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NestedCallView {
    /// 脚本里的序号 key（`c1`、`c2`……）。
    pub id: String,
    pub tool: String,
    /// 收窄过的参数（[`bounded_args`]）：认得出主参数的工具照常画头一行。
    #[serde(default)]
    pub args: Value,
    pub state: ToolCallState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 子调用完整输出的入口（`artifact://…/result`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// 子调用的状态：结果不明优先，其次宿主报错（`ok = false`），再看工具自己报的 status。
pub fn state_of(ok: bool, status: Option<ToolResultStatus>, uncertain: bool) -> ToolCallState {
    if uncertain || status == Some(ToolResultStatus::Uncertain) {
        return ToolCallState::Uncertain;
    }
    if !ok || status == Some(ToolResultStatus::Failed) {
        return ToolCallState::Failed;
    }
    ToolCallState::Completed
}

/// 收窄参数：长字符串截断、数组与对象只留前几项、只留几层。`write` 的整份 content 不会跟着
/// 每一帧走，`path` / `command` 这些主参数照样在。
pub fn bounded_args(value: &Value) -> Value {
    bound(value, ARG_DEPTH)
}

fn bound(value: &Value, depth: usize) -> Value {
    match value {
        Value::String(text) => Value::String(cut_chars(text, ARG_STRING_CHARS)),
        Value::Array(items) if depth == 0 => json!(format!("[… {} 项]", items.len())),
        Value::Object(map) if depth == 0 => json!(format!("{{… {} 项}}", map.len())),
        Value::Array(items) => {
            let mut out: Vec<Value> = items
                .iter()
                .take(ARG_ITEMS)
                .map(|item| bound(item, depth - 1))
                .collect();
            if items.len() > ARG_ITEMS {
                out.push(json!(format!("… 还有 {} 项", items.len() - ARG_ITEMS)));
            }
            Value::Array(out)
        }
        Value::Object(map) => {
            let mut out: Map<String, Value> = map
                .iter()
                .take(ARG_ITEMS)
                .map(|(key, item)| (key.clone(), bound(item, depth - 1)))
                .collect();
            if map.len() > ARG_ITEMS {
                out.insert("…".into(), json!(map.len() - ARG_ITEMS));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// 外层 codemode 结果（`output.json` 的 `result`，或已经 [`display_result`] 过的）里的子调用。
/// 不是 codemode 结果（没有 `calls` 数组）就是 `None`。
pub fn calls_of(result: &Value) -> Option<Vec<NestedCallView>> {
    let calls = result.get("calls")?.as_array()?;
    Some(
        calls
            .iter()
            .take(MAX_VIEW_CALLS)
            .filter_map(call_of)
            .collect(),
    )
}

fn call_of(call: &Value) -> Option<NestedCallView> {
    let id = call.get("id")?.as_str()?.to_owned();
    let tool = call.get("tool")?.as_str()?.to_owned();
    // 已经是视图（再收窄一遍无妨，结果不变）。
    if let Some(state) = call
        .get("state")
        .and_then(|state| serde_json::from_value::<ToolCallState>(state.clone()).ok())
    {
        return Some(NestedCallView {
            id,
            tool,
            args: bounded_args(call.get("args").unwrap_or(&Value::Null)),
            state,
            elapsed_ms: call.get("elapsed_ms").and_then(Value::as_u64),
            error: text_of(call, "error").map(|error| cut_bytes(&error, ERROR_VIEW_BYTES)),
            output: text_of(call, "output"),
        });
    }
    // runtime 的 NestedCall：`arguments` 是收窄过的参数；旧记录只有截断过的 JSON 文本 `args`，
    // 解得开就用，解不开原样当一段文字。
    let args = match call.get("arguments") {
        Some(arguments) => bounded_args(arguments),
        None => match call.get("args").and_then(Value::as_str) {
            Some(text) => serde_json::from_str::<Value>(text)
                .map(|value| bounded_args(&value))
                .unwrap_or_else(|_| Value::String(cut_chars(text, ARG_STRING_CHARS))),
            None => Value::Null,
        },
    };
    let ok = call.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let status = call
        .get("status")
        .and_then(|status| serde_json::from_value(status.clone()).ok());
    let uncertain = call
        .get("uncertain")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(NestedCallView {
        id,
        tool,
        args,
        state: state_of(ok, status, uncertain),
        elapsed_ms: call.get("elapsed_ms").and_then(Value::as_u64),
        error: text_of(call, "error").map(|error| cut_bytes(&error, ERROR_VIEW_BYTES)),
        output: text_of(call, "output"),
    })
}

fn text_of(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

/// 外层 codemode 结果收窄成界面要的那一份：`{calls: [视图], error?}`。不是 codemode 结果就是
/// `Value::Null`（与没读到一样）。
pub fn display_result(result: &Value) -> Value {
    let Some(calls) = calls_of(result) else {
        return Value::Null;
    };
    let mut out = json!({ "calls": calls });
    if let Some(error) = text_of(result, "error") {
        out["error"] = json!(cut_bytes(&error, ERROR_VIEW_BYTES));
    }
    out
}

fn cut_chars(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

fn cut_bytes(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncertain_wins_then_host_failure_then_the_tools_own_status() {
        use ToolCallState::*;
        assert_eq!(
            state_of(true, Some(ToolResultStatus::Completed), false),
            Completed
        );
        assert_eq!(state_of(true, None, false), Completed);
        // bash 非零退出：宿主正常交回，工具自己说失败。
        assert_eq!(
            state_of(true, Some(ToolResultStatus::Failed), false),
            Failed
        );
        assert_eq!(state_of(false, None, false), Failed);
        assert_eq!(state_of(false, None, true), Uncertain);
        assert_eq!(
            state_of(true, Some(ToolResultStatus::Uncertain), false),
            Uncertain
        );
    }

    #[test]
    fn bounded_args_keep_the_main_keys_and_drop_bulk() {
        let content = "x".repeat(10_000);
        let args = json!({ "path": "/tmp/a.txt", "content": content, "edits": [1, 2, 3] });
        let bounded = bounded_args(&args);
        assert_eq!(bounded["path"], "/tmp/a.txt");
        let kept = bounded["content"].as_str().unwrap();
        assert!(
            kept.chars().count() <= ARG_STRING_CHARS + 1,
            "{}",
            kept.len()
        );
        assert!(kept.ends_with('…'));
        assert_eq!(bounded["edits"], json!([1, 2, 3]));
        // 再收窄一遍不变：直播与补读各收一次也一样。
        assert_eq!(bounded_args(&bounded), bounded);

        let many: Vec<u32> = (0..40).collect();
        let bounded = bounded_args(&json!({ "items": many }));
        let items = bounded["items"].as_array().unwrap();
        assert_eq!(items.len(), ARG_ITEMS + 1);
        assert_eq!(items[ARG_ITEMS], "… 还有 24 项");
        let deep = bounded_args(&json!({ "a": { "b": { "c": { "d": 1 } } } }));
        assert_eq!(deep["a"]["b"]["c"], "{… 1 项}");
    }

    #[test]
    fn runtime_calls_become_views_and_old_records_still_read() {
        let result = json!({
            "calls": [
                { "id": "c1", "tool": "read", "args": "{\"path\":\"a\"}",
                  "arguments": { "path": "a" }, "ok": true, "status": "completed",
                  "output": "artifact://1/2/1/result", "elapsed_ms": 12 },
                { "id": "c2", "tool": "bash", "args": "{\"command\":\"false\"}", "ok": true,
                  "status": "failed", "elapsed_ms": 3 },
                { "id": "c3", "tool": "edit", "args": "{\"path\":\"b\",\"edits\":[…（截断）",
                  "ok": false, "uncertain": true, "elapsed_ms": 0, "error": "e".repeat(2000) },
            ],
            "error": null,
        });
        let calls = calls_of(&result).unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].args, json!({ "path": "a" }));
        assert_eq!(calls[0].state, ToolCallState::Completed);
        assert_eq!(calls[0].output.as_deref(), Some("artifact://1/2/1/result"));
        // 旧记录没有 `arguments`：从 args 文本解出来。
        assert_eq!(calls[1].args, json!({ "command": "false" }));
        assert_eq!(calls[1].state, ToolCallState::Failed);
        // 截断过、解不开的：原样当文字。
        assert!(calls[2].args.is_string());
        assert_eq!(calls[2].state, ToolCallState::Uncertain);
        assert!(calls[2].error.as_ref().unwrap().len() <= ERROR_VIEW_BYTES + 3);

        let shown = display_result(&result);
        assert_eq!(calls_of(&shown).unwrap(), calls, "视图再读一遍不变");
        assert!(shown.get("error").is_none());
        assert_eq!(display_result(&json!({ "exit_code": 0 })), Value::Null);
        assert_eq!(display_result(&Value::Null), Value::Null);
    }
}
