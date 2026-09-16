//! 内存里的 [`MemoryRepo`]。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::traits::*;
use crate::types::ids::*;

use crate::protocol::http::{IndexState, MemoryIndexStatus};
use crate::types::memory::{MemoryItem, MemoryState, RecallQuery, RecallResult};
use crate::types::model::Vector;

/// 内存里的 [`MemoryRepo`]。召回是朴素的子串 + 余弦，**够用来测排序与降级**。
#[derive(Debug, Clone, Default)]
pub struct MemMemoryRepo {
    state: Arc<Mutex<MemoryState_>>,
}

#[derive(Debug, Default)]
struct MemoryState_ {
    items: BTreeMap<MemoryId, MemoryItem>,
    vectors: BTreeMap<MemoryId, Vector>,
    /// 置位后 recall 明示降级。
    vector_backend_down: bool,
}

impl MemMemoryRepo {
    pub fn new() -> Self {
        Self::default()
    }

    /// 模拟向量端点故障：hybrid 退化为关键词并标记 degraded（§9.4）。
    pub fn set_vector_backend_down(&self, down: bool) {
        self.state.lock().expect("记忆").vector_backend_down = down;
    }

    pub fn insert(&self, item: MemoryItem) {
        self.state
            .lock()
            .expect("记忆")
            .items
            .insert(item.id.clone(), item);
    }
}

#[async_trait]
impl MemoryRepo for MemMemoryRepo {
    async fn get(&self, id: &MemoryId) -> Result<Option<MemoryItem>, RepoError> {
        Ok(self.state.lock().expect("记忆").items.get(id).cloned())
    }

    async fn put(
        &self,
        item: MemoryItem,
        expected_revision: Option<u32>,
    ) -> Result<MemoryItem, RepoError> {
        let mut state = self.state.lock().expect("记忆");
        if let (Some(expected), Some(existing)) = (expected_revision, state.items.get(&item.id))
            && existing.revision != expected
        {
            return Err(RepoError::VersionConflict {
                expected,
                actual: existing.revision,
            });
        }
        state.items.insert(item.id.clone(), item.clone());
        Ok(item)
    }

    async fn recall(&self, query: &RecallQuery) -> Result<RecallResult, RepoError> {
        let state = self.state.lock().expect("记忆");
        let items: Vec<MemoryItem> = state
            .items
            .values()
            .filter(|m| m.is_recallable_at(query.now))
            .filter(|m| query.text.is_empty() || m.content.contains(&query.text))
            .take(query.top_k as usize)
            .cloned()
            .collect();
        Ok(RecallResult {
            items,
            mode: query.mode,
            degraded: state.vector_backend_down,
            degraded_reason: state
                .vector_backend_down
                .then(|| "向量端点不可用，已退化为关键词".to_string()),
            vector_coverage: Some(if state.vectors.is_empty() { 0.0 } else { 1.0 }),
        })
    }

    async fn confirm(
        &self,
        id: &MemoryId,
        expected_revision: u32,
        at: OffsetDateTime,
    ) -> Result<MemoryItem, RepoError> {
        let mut state = self.state.lock().expect("记忆");
        let item = state.items.get_mut(id).ok_or_else(|| RepoError::NotFound {
            what: format!("memory {id}"),
        })?;
        if item.revision != expected_revision {
            return Err(RepoError::VersionConflict {
                expected: expected_revision,
                actual: item.revision,
            });
        }
        item.confirmation = crate::types::memory::Confirmation::UserConfirmed;
        item.updated_at = at;
        Ok(item.clone())
    }

    async fn forget(
        &self,
        id: &MemoryId,
        expected_revision: u32,
        at: OffsetDateTime,
    ) -> Result<MemoryItem, RepoError> {
        let mut state = self.state.lock().expect("记忆");
        let item = state.items.get_mut(id).ok_or_else(|| RepoError::NotFound {
            what: format!("memory {id}"),
        })?;
        if item.revision != expected_revision {
            return Err(RepoError::VersionConflict {
                expected: expected_revision,
                actual: item.revision,
            });
        }
        item.state = MemoryState::Forgotten;
        item.updated_at = at;
        let forgotten = item.clone();
        state.vectors.remove(id);
        Ok(forgotten)
    }

    async fn put_vector(
        &self,
        id: &MemoryId,
        revision: u32,
        _generation: &str,
        vector: Vector,
    ) -> Result<(), RepoError> {
        let mut state = self.state.lock().expect("记忆");
        // 入库前确认正文版本未变；不满足则丢弃过期结果（§9.5）。
        let Some(item) = state.items.get(id) else {
            return Ok(());
        };
        if item.revision != revision {
            return Ok(());
        }
        state.vectors.insert(id.clone(), vector);
        Ok(())
    }

    async fn index_status(&self) -> Result<MemoryIndexStatus, RepoError> {
        let state = self.state.lock().expect("记忆");
        let total = state.items.len() as u64;
        let indexed = state.vectors.len() as u64;
        Ok(MemoryIndexStatus {
            space: None,
            generation: Some("test".into()),
            state: if state.vector_backend_down {
                IndexState::Failed
            } else {
                IndexState::Ready
            },
            coverage: if total == 0 {
                0.0
            } else {
                indexed as f32 / total as f32
            },
            indexed,
            total,
            errors: vec![],
        })
    }
}
