//! codemode 子调用给界面的那两份（`docs/codemode-bash.md` §3.5）：
//!
//! - **跑完了**：[`wanted`] + [`load_results`] 按 codemode 结果 entry 的 `data.output` 读外层 `output.json`，交给投影
//!   （[`Projection::attach_result`](komo_kernel::durable::legacy_events::Projection::attach_result)）；
//!   补读与直播都调它，读的是同一份落盘的事实，所以帧照样逐个相同；
//! - **还在跑**：[`LiveNested`] 从 `komo.live.nestedTools` 加 `komo.nested` 成员合成
//!   [`SseEvent::NestedCalls`](komo_kernel::protocol::sse::SseEvent::NestedCalls) 的整份快照，只在 SSE 上。
//!
//! 两份都经 [`calls_of`] 归一，形状与状态判定只有一处。这里只读，不进 transcript，也不是权威。

use std::collections::HashMap;
use std::path::Path;

use komo_kernel::durable::legacy_events::wants_result;
use komo_kernel::durable::legacy_ids::run_id;
use komo_kernel::durable::{EntryId, EntryRecord, Storage, TaskId};
use komo_kernel::protocol::nested::{NestedCallView, calls_of, state_of};
use komo_kernel::traits::ToolOutputStore;
use komo_kernel::types::ids::{RunId, SessionId, ToolCallId};
use komo_kernel::types::refs::OutputRef;
use komo_kernel::types::status::ToolCallState;
use komo_runtime::durable::harness::live::{LiveState, NestedToolSlot, SlotStatus, live_address};
use komo_runtime::durable::harness::nested::{NestedDoc, nested_address};
use komo_store::{FileToolOutputStore, SessionPaths};
use serde::Deserialize;
use serde_json::{Value, json};

use super::tree::load_tree;

/// 这些 entry 里要读外层结果的那几条（codemode 的结果 entry）与它们的输出引用。
pub fn wanted<'a>(entries: impl IntoIterator<Item = &'a EntryRecord>) -> Vec<(EntryId, OutputRef)> {
    entries
        .into_iter()
        .filter_map(|entry| Some((entry.id, wants_result(entry)?)))
        .collect()
}

/// 读 [`wanted`] 挑出来的外层 `result`（哈希核对过）。`sessions_dir` 没有（还没接上配置）或读不到的
/// 就不给——投影照旧是 `Null`，界面只是少画子调用。
pub async fn load_results(
    sessions_dir: Option<&Path>,
    session: &str,
    wanted: Vec<(EntryId, OutputRef)>,
) -> HashMap<EntryId, Value> {
    let mut results = HashMap::new();
    let Some(sessions_dir) = sessions_dir else {
        return results;
    };
    if wanted.is_empty() {
        return results;
    }
    let id = SessionId::from_raw(session);
    let store = FileToolOutputStore::new(SessionPaths::new(sessions_dir, &id), id);
    for (entry, output) in wanted {
        match store.open(&output).await {
            Ok(verified) => {
                results.insert(entry, verified.body.result);
            }
            Err(error) => {
                tracing::debug!(session, %entry, %error, "读不到 codemode 外层结果，不画子调用");
            }
        }
    }
    results
}

/// 一帧要推的子调用快照。
#[derive(Debug, Clone, PartialEq)]
pub struct NestedFrame {
    pub run: RunId,
    pub call: ToolCallId,
    pub calls: Vec<NestedCallView>,
}

/// 一个 conversation 的直播状态：`komo.nested` 成员的缓存与每个外层调用上次推出去的快照。
#[derive(Debug, Default)]
pub struct LiveNested {
    docs: HashMap<(TaskId, String), NestedDoc>,
    sent: HashMap<String, Vec<NestedCallView>>,
}

impl LiveNested {
    /// `komo.live` 变了：补读缺的成员，返回内容变了的外层调用的快照。
    pub async fn update(&mut self, storage: &dyn Storage, live: &LiveState) -> Vec<NestedFrame> {
        let slots = live.nested_tools.as_deref().unwrap_or_default();
        self.docs
            .retain(|(parent, _), _| slots.iter().any(|slot| slot.parent_task_id == *parent));
        for slot in slots {
            let key = (slot.parent_task_id, key_of(slot).to_owned());
            let settled = matches!(slot.slot.status, SlotStatus::Done | SlotStatus::Failed);
            let stale = match self.docs.get(&key) {
                None => true,
                Some(doc) => settled && doc.result.is_none(),
            };
            if !stale {
                continue;
            }
            match storage.find_document(&nested_address(key.0, &key.1)).await {
                Ok(Some(record)) => match NestedDoc::deserialize(&record.value) {
                    Ok(doc) => {
                        self.docs.insert(key, doc);
                    }
                    Err(error) => tracing::debug!(%error, "komo.nested 成员解不开"),
                },
                // 退役了（外层刚终结）：结果马上由外层的 tool.result 带到。
                Ok(None) => {}
                Err(error) => tracing::debug!(%error, "读不到 komo.nested 成员"),
            }
        }
        let current = views(live, &self.docs);
        let mut frames = Vec::new();
        let Some(run) = live
            .run
            .as_ref()
            .and_then(|run| run.inputs.first())
            .map(|first| run_id(*first))
        else {
            self.sent.clear();
            return frames;
        };
        for (call, calls) in &current {
            if self.sent.get(call) != Some(calls) {
                frames.push(NestedFrame {
                    run: run.clone(),
                    call: ToolCallId::from_raw(call),
                    calls: calls.clone(),
                });
            }
        }
        self.sent = current.into_iter().collect();
        frames
    }
}

/// 子调用槽位的 key（`{外层调用}/c3` 的 `c3`）。
fn key_of(slot: &NestedToolSlot) -> &str {
    let id = slot.slot.call_id.as_str();
    id.rsplit_once('/').map_or(id, |(_, key)| key)
}

/// 纯函数：`komo.live` 的子调用槽位加已知的成员 → 每个外层调用（按调用 ID）的视图，按槽位次序。
/// 外层槽位找不到（这一轮已经收尾）的子调用不画。
pub fn views(
    live: &LiveState,
    docs: &HashMap<(TaskId, String), NestedDoc>,
) -> Vec<(String, Vec<NestedCallView>)> {
    let outer: HashMap<TaskId, &str> = live
        .tools
        .iter()
        .flatten()
        .filter_map(|slot| Some((slot.task_id?, slot.call_id.as_str())))
        .collect();
    let mut out: Vec<(String, Vec<Value>)> = Vec::new();
    for slot in live.nested_tools.iter().flatten() {
        let Some(call) = outer.get(&slot.parent_task_id) else {
            continue;
        };
        let key = key_of(slot);
        let doc = docs.get(&(slot.parent_task_id, key.to_owned()));
        let raw = view_value(key, slot, doc);
        match out.iter_mut().find(|(id, _)| id == call) {
            Some((_, calls)) => calls.push(raw),
            None => out.push(((*call).to_owned(), vec![raw])),
        }
    }
    out.into_iter()
        .map(|(call, calls)| {
            let calls = calls_of(&json!({ "calls": calls })).unwrap_or_default();
            (call, calls)
        })
        .collect()
}

/// 一个子调用的视图（未归一）。状态与历史同一个判定：结果写进成员之前看槽位，之后看结果。
fn view_value(key: &str, slot: &NestedToolSlot, doc: Option<&NestedDoc>) -> Value {
    let result = doc.and_then(|doc| doc.result.as_ref());
    let state = match (slot.slot.status, result) {
        (_, Some(result)) => state_of(
            result.script_result().is_ok(),
            result.status(),
            result.uncertain,
        ),
        (SlotStatus::Pending, None) => ToolCallState::Planned,
        (SlotStatus::Running | SlotStatus::Done, None) => ToolCallState::Started,
        // 任务终结了却没写结果（faulted / orphaned）：做没做过不知道。
        (SlotStatus::Failed, None) => ToolCallState::Uncertain,
    };
    let mut value = json!({
        "id": key,
        "tool": slot.slot.name,
        "args": doc.map_or(Value::Null, |doc| doc.arguments.clone()),
        "state": state,
    });
    if let Some(result) = result {
        if let Some(error) = &result.error {
            value["error"] = json!(error);
        }
        if let Some(output) = &result.output {
            value["output"] = json!(output);
        }
    }
    value
}

/// 重连时的直播快照：这个 Session 每个 conversation 当前还在跑的 codemode 调用的子调用。
pub async fn snapshot(storage: &dyn Storage, session: &str) -> Vec<NestedFrame> {
    let tree = match load_tree(storage, session).await {
        Ok(tree) => tree,
        Err(error) => {
            tracing::debug!(session, %error, "子调用快照读不到会话树");
            return Vec::new();
        }
    };
    let mut frames = Vec::new();
    for branch in tree.branches {
        let address = live_address(branch.conversation.id);
        let Ok(Some(record)) = storage.find_document(&address).await else {
            continue;
        };
        let Ok(live) = LiveState::deserialize(&record.value) else {
            continue;
        };
        if live.nested_tools.as_deref().is_none_or(<[_]>::is_empty) {
            continue;
        }
        frames.extend(LiveNested::default().update(storage, &live).await);
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::durable::SubmissionId;
    use komo_runtime::durable::harness::live::{RunControl, ToolSlot};
    use komo_runtime::durable::harness::nested::NestedResult;

    fn slot(call: &str, name: &str, task: u64, status: SlotStatus) -> ToolSlot {
        let mut slot = ToolSlot::pending(call, name);
        slot.task_id = Some(TaskId::new(task));
        slot.status = status;
        slot
    }

    fn nested(key: &str, name: &str, task: u64, status: SlotStatus) -> NestedToolSlot {
        NestedToolSlot {
            parent_task_id: TaskId::new(9),
            slot: slot(&format!("m1/{key}"), name, task, status),
        }
    }

    fn live(nested: Vec<NestedToolSlot>) -> LiveState {
        LiveState {
            run: Some(RunControl {
                task: TaskId::new(5),
                inputs: vec![SubmissionId::new(3)],
                rounds: 0,
                tokens: 0,
                started_at: 0,
            }),
            tools: Some(vec![slot("m1", "codemode", 9, SlotStatus::Running)]),
            nested_tools: Some(nested),
            ..LiveState::default()
        }
    }

    fn doc(task: u64, name: &str, arguments: Value, result: Option<NestedResult>) -> NestedDoc {
        NestedDoc {
            task_id: TaskId::new(task),
            name: name.into(),
            arguments,
            result,
        }
    }

    #[test]
    fn live_views_follow_the_slot_until_the_member_has_a_result() {
        let state = live(vec![
            nested("c1", "read", 10, SlotStatus::Done),
            nested("c2", "bash", 11, SlotStatus::Running),
            nested("c3", "write", 12, SlotStatus::Pending),
            nested("c4", "edit", 13, SlotStatus::Failed),
        ]);
        let mut docs = HashMap::new();
        docs.insert(
            (TaskId::new(9), "c1".to_owned()),
            doc(
                10,
                "read",
                json!({ "path": "a.txt" }),
                Some(NestedResult {
                    value: Some(json!({ "status": "completed", "text": "hi", "result": {} })),
                    error: None,
                    uncertain: false,
                    images: Vec::new(),
                    output: Some("artifact://1/10/1/result".into()),
                }),
            ),
        );
        docs.insert(
            (TaskId::new(9), "c2".to_owned()),
            doc(11, "bash", json!({ "command": "x".repeat(1000) }), None),
        );
        let shown = views(&state, &docs);
        assert_eq!(shown.len(), 1);
        let (call, calls) = &shown[0];
        assert_eq!(call, "m1");
        let states: Vec<_> = calls.iter().map(|call| call.state).collect();
        assert_eq!(
            states,
            vec![
                ToolCallState::Completed,
                ToolCallState::Started,
                ToolCallState::Planned,
                ToolCallState::Uncertain,
            ]
        );
        assert_eq!(calls[0].id, "c1");
        assert_eq!(calls[0].args, json!({ "path": "a.txt" }));
        assert_eq!(calls[0].output.as_deref(), Some("artifact://1/10/1/result"));
        // 参数照样收窄：一大段 command 不跟着每一帧走。
        assert!(calls[1].args["command"].as_str().unwrap().len() < 400);
        // 成员还没读到：工具名照样有，参数空着。
        assert_eq!(calls[2].tool, "write");
        assert!(calls[2].args.is_null());
    }

    #[test]
    fn nested_slots_without_their_outer_slot_are_not_drawn() {
        let mut state = live(vec![nested("c1", "read", 10, SlotStatus::Running)]);
        state.tools = Some(Vec::new());
        assert!(views(&state, &HashMap::new()).is_empty());
    }

    #[test]
    fn a_failed_member_reads_like_the_history_does() {
        let state = live(vec![nested("c1", "edit", 10, SlotStatus::Done)]);
        let mut docs = HashMap::new();
        docs.insert(
            (TaskId::new(9), "c1".to_owned()),
            doc(
                10,
                "edit",
                json!({ "path": "b" }),
                Some(NestedResult {
                    value: None,
                    error: Some("old_text 没找到".into()),
                    uncertain: false,
                    images: Vec::new(),
                    output: None,
                }),
            ),
        );
        let calls = &views(&state, &docs)[0].1;
        assert_eq!(calls[0].state, ToolCallState::Failed);
        assert_eq!(calls[0].error.as_deref(), Some("old_text 没找到"));
    }
}
