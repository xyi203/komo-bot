//! 内存里的 [`RunQueue`]。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::traits::*;
use crate::types::ids::*;

use crate::types::status::Claimed;

/// 内存里的 [`RunQueue`]。**条件领取与代次递增都是真的**——并发领取测试要它。
#[derive(Debug, Clone, Default)]
pub struct MemRunQueue {
    state: Arc<Mutex<QueueState>>,
}

#[derive(Debug, Default)]
struct QueueState {
    queued: Vec<RunId>,
    generations: BTreeMap<RunId, u64>,
}

impl MemRunQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enqueue(&self, run: RunId) {
        let mut state = self.state.lock().expect("队列");
        if !state.queued.contains(&run) {
            state.queued.push(run);
        }
    }

    pub fn depth(&self) -> usize {
        self.state.lock().expect("队列").queued.len()
    }
}

#[async_trait]
impl RunQueue for MemRunQueue {
    async fn claim(&self, _executor: &ExecutorId) -> Result<Option<Claimed>, StoreError> {
        let mut state = self.state.lock().expect("队列");
        let Some(run) = (!state.queued.is_empty()).then(|| state.queued.remove(0)) else {
            return Ok(None);
        };
        let generation = state.generations.entry(run.clone()).or_insert(0);
        *generation += 1;
        Ok(Some(Claimed {
            run,
            generation: *generation,
        }))
    }

    async fn claim_run(
        &self,
        run: &RunId,
        _executor: &ExecutorId,
    ) -> Result<Option<Claimed>, StoreError> {
        let mut state = self.state.lock().expect("队列");
        // 条件更新：只有还排在队列里的 Run 领得走，所以两个执行者同时来只有一个成功。
        let Some(position) = state.queued.iter().position(|queued| queued == run) else {
            return Ok(None);
        };
        state.queued.remove(position);
        let generation = state.generations.entry(run.clone()).or_insert(0);
        *generation += 1;
        Ok(Some(Claimed {
            run: run.clone(),
            generation: *generation,
        }))
    }

    async fn release(&self, claimed: &Claimed) -> Result<(), StoreError> {
        let mut state = self.state.lock().expect("队列");
        // 旧代次不能交还名额。
        if state.generations.get(&claimed.run) == Some(&claimed.generation)
            && !state.queued.contains(&claimed.run)
        {
            state.queued.push(claimed.run.clone());
        }
        Ok(())
    }
}
