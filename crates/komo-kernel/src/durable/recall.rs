//! 记忆召回块在 entry 上的形状（`docs/memory.md` §9.4「按 Run 召回」）。
//!
//! 每个 Run 在当前段里召回一次，落成一条 [`MEMORY_RECALL_ENTRY_KIND`] entry，追加在它的输入之后：
//! `model` 是一条带 [`HOST_ORIGIN_KEY`](super::message::HOST_ORIGIN_KEY) 的 user 消息（筛空时没有
//! `model`，不进上下文），`data` 是 [`RecallData`]。被遗忘 / 失效的条目由一条只带 `edits` 的
//! [`MEMORY_REVOKE_ENTRY_KIND`] entry 从旧块的视图里去掉（同 `komo.decay`）。

use serde::{Deserialize, Serialize};

use super::ids::SubmissionId;
use super::records::EntryRecord;
use crate::types::memory::RecallDiagnostics;
use crate::types::turn::MemoryUse;

/// 召回块 entry 的 `kind`。
pub const MEMORY_RECALL_ENTRY_KIND: &str = "komo.memory.recall";
/// 改写旧召回块的 write entry 的 `kind`：只带 `edits`。
pub const MEMORY_REVOKE_ENTRY_KIND: &str = "komo.memory.revoke";
/// 召回块那条 user 消息的 [`HOST_ORIGIN_KEY`](super::message::HOST_ORIGIN_KEY) 值。
pub const MEMORY_ORIGIN: &str = "memory";

/// 召回块 entry 的 `data`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecallData {
    /// 这一块属于哪个 Run（首个输入的 submission）。
    pub submission: SubmissionId,
    /// 块里展示的条目与版本；筛空时为空。
    #[serde(default)]
    pub uses: Vec<MemoryUse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall: Option<RecallDiagnostics>,
    /// 召回时刻（毫秒）。
    #[serde(default)]
    pub at: i64,
}

impl RecallData {
    /// 一条召回块 entry 的 `data`；不是召回块或形状不对都是 `None`。
    pub fn of(entry: &EntryRecord) -> Option<Self> {
        if entry.kind != MEMORY_RECALL_ENTRY_KIND {
            return None;
        }
        serde_json::from_value(entry.data.clone()?).ok()
    }
}
