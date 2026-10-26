//! 上下文的边界在线上捕获；不可变范围在线外读取，按 conversation 复用。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use komo_kernel::compaction::OnlineState;
use komo_kernel::durable::compaction::OnlineFold;
use komo_kernel::durable::context::ContextRange;
use komo_kernel::durable::storage::EntryQuery;
use komo_kernel::durable::{
    ContextView, ConversationId, EntryId, EntryRecord, ScanOrder, Storage, StorageWrite,
};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;

use super::error::DurableError;
use super::line::{Line, Publication};

const PAGE: usize = 256;
const RETENTION: Duration = Duration::from_secs(10 * 60);

#[derive(Clone)]
struct Bounds {
    head: Option<EntryRecord>,
    tail: Option<EntryId>,
}

impl Bounds {
    fn min(&self) -> Option<EntryId> {
        self.head.as_ref().and_then(|head| head.head)
    }

    fn head_id(&self) -> Option<EntryId> {
        self.head.as_ref().map(|head| head.id)
    }
}

/// 调用者共享不可变记录与推导结果，不为每次读取克隆整段历史。
#[derive(Clone)]
pub struct ContextSnapshot {
    pub entries: Arc<Vec<EntryRecord>>,
    pub view: Arc<ContextView>,
    online: OnlineFold,
}

impl ContextSnapshot {
    pub fn online_state(&self, refused_after: Option<EntryId>) -> OnlineState {
        self.online.state(refused_after)
    }
}

struct CachedRange {
    bounds: Bounds,
    entries: Arc<Vec<EntryRecord>>,
    derived: ContextRange,
}

#[derive(Default)]
struct Cached {
    range: Option<CachedRange>,
    /// 全历史账本独立于活动范围，也独立于 request / summary 的历史 cutoff。
    online: Option<(Option<EntryId>, OnlineFold)>,
}

struct Slot {
    valid: AtomicBool,
    cached: AsyncMutex<Cached>,
}

#[derive(Default)]
pub(super) struct ContextCache {
    slots: Mutex<HashMap<ConversationId, (Instant, Arc<Slot>)>>,
}

impl ContextCache {
    fn slot(&self, conversation: ConversationId) -> Arc<Slot> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let (used, slot) = slots.entry(conversation).or_insert_with(|| {
            (
                Instant::now(),
                Arc::new(Slot {
                    valid: AtomicBool::new(true),
                    cached: AsyncMutex::new(Cached::default()),
                }),
            )
        });
        *used = Instant::now();
        slot.clone()
    }

    pub fn expire(&self, busy: impl Fn(ConversationId) -> bool) {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|conversation, (used, slot)| {
                used.elapsed() < RETENTION || Arc::strong_count(slot) > 1 || busy(*conversation)
            });
    }

    pub fn observe(&self, publication: &Publication) {
        for write in &publication.writes {
            if let StorageWrite::PurgeConversations { conversations } = write {
                // Gateway 按 Session 整组 purge（包括所有 fork 后代）。旧的在飞读取也失效。
                let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
                for conversation in conversations {
                    if let Some((_, slot)) = slots.remove(conversation) {
                        slot.valid.store(false, Ordering::Release);
                    }
                }
            }
        }
    }

    pub async fn snapshot(
        &self,
        line: &Line,
        conversation: ConversationId,
    ) -> Result<ContextSnapshot, DurableError> {
        self.snapshot_inner(line, conversation)
            .await
            .map_err(|error| line.settle_error(error))
    }

    async fn snapshot_inner(
        &self,
        line: &Line,
        conversation: ConversationId,
    ) -> Result<ContextSnapshot, DurableError> {
        let slot = self.slot(conversation);
        let mut cached = slot.cached.lock().await;
        let bounds = capture_bounds(line, conversation, None).await?;
        // 首次冷读顺带折全历史，之后只折新增记录；压缩不能抹掉缓存债。
        if cached
            .online
            .as_ref()
            .is_none_or(|(tail, _)| *tail != bounds.tail)
        {
            let previous_tail = cached.online.as_ref().and_then(|(tail, _)| *tail);
            let min = previous_tail.map(after);
            let added = scan(line.storage().as_ref(), conversation, min, bounds.tail).await?;
            let mut fold = cached
                .online
                .take()
                .map(|(_, fold)| fold)
                .unwrap_or_default();
            fold.extend(&added);
            cached.online = Some((bounds.tail, fold));
            // 冷读已经拿到了所有 entry，不再为了活动范围扫第二遍。
            if cached.range.is_none() {
                let entries: Arc<Vec<EntryRecord>> = Arc::new(
                    added
                        .into_iter()
                        .filter(|entry| bounds.min().is_none_or(|min| entry.id >= min))
                        .collect(),
                );
                cached.range = Some(CachedRange {
                    derived: ContextRange::new(&entries),
                    entries,
                    bounds: bounds.clone(),
                });
            } else if let Some(range) = cached.range.as_mut()
                && range.bounds.tail == previous_tail
                && range.bounds.head_id() == bounds.head_id()
            {
                extend_range(range, &added);
                range.bounds = bounds.clone();
            }
        }
        let range = load_range(
            line.storage().as_ref(),
            conversation,
            bounds,
            &mut cached.range,
        )
        .await?;
        check_slot(&slot)?;
        Ok(ContextSnapshot {
            entries: range.entries.clone(),
            view: range.derived.view.clone(),
            online: cached
                .online
                .as_ref()
                .expect("online fold initialized above")
                .1
                .clone(),
        })
    }

    pub async fn view(
        &self,
        line: &Line,
        conversation: ConversationId,
        at: Option<EntryId>,
    ) -> Result<Arc<ContextView>, DurableError> {
        self.view_inner(line, conversation, at)
            .await
            .map_err(|error| line.settle_error(error))
    }

    async fn view_inner(
        &self,
        line: &Line,
        conversation: ConversationId,
        at: Option<EntryId>,
    ) -> Result<Arc<ContextView>, DurableError> {
        let slot = self.slot(conversation);
        let mut cached = slot.cached.lock().await;
        let bounds = capture_bounds(line, conversation, at).await?;
        let range = load_range(
            line.storage().as_ref(),
            conversation,
            bounds,
            &mut cached.range,
        )
        .await?;
        check_slot(&slot)?;
        Ok(range.derived.view.clone())
    }
}

fn check_slot(slot: &Slot) -> Result<(), DurableError> {
    if slot.valid.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err(DurableError::rejected(
            "conversation was purged while reading context",
        ))
    }
}

fn after(id: EntryId) -> EntryId {
    EntryId::new(id.get() + 1)
}

async fn capture_bounds(
    line: &Line,
    conversation: ConversationId,
    at: Option<EntryId>,
) -> Result<Bounds, DurableError> {
    let storage = line.storage().clone();
    line.read_on_line(move || {
        Box::pin(async move {
            let query = EntryQuery {
                max: at,
                ..EntryQuery::new(conversation)
            };
            let tail = storage
                .scan_entries(&query, 1, None)
                .await?
                .items
                .first()
                .map(|entry| entry.id);
            let head = match tail {
                Some(tail) => {
                    storage
                        .find_latest_head_marker(conversation, Some(tail))
                        .await?
                }
                None => None,
            };
            Ok(Bounds { head, tail })
        })
    })
    .await
}

async fn load_range<'a>(
    storage: &dyn Storage,
    conversation: ConversationId,
    bounds: Bounds,
    cached: &'a mut Option<CachedRange>,
) -> Result<&'a CachedRange, DurableError> {
    if let Some(previous) = cached.as_mut()
        && previous.bounds.head_id() == bounds.head_id()
    {
        if previous.bounds.tail == bounds.tail {
            return Ok(cached.as_ref().unwrap());
        }
        if bounds.tail < previous.bounds.tail {
            Arc::make_mut(&mut previous.entries).retain(|entry| Some(entry.id) <= bounds.tail);
            previous.derived = ContextRange::new(&previous.entries);
        } else {
            let min = previous.bounds.tail.map(after);
            let added = scan(storage, conversation, min, bounds.tail).await?;
            extend_range(previous, &added);
        }
        previous.bounds = bounds;
    } else {
        let entries = Arc::new(scan(storage, conversation, bounds.min(), bounds.tail).await?);
        *cached = Some(CachedRange {
            derived: ContextRange::new(&entries),
            entries,
            bounds,
        });
    }
    Ok(cached.as_ref().unwrap())
}

fn extend_range(previous: &mut CachedRange, added: &[EntryRecord]) {
    let entries = Arc::make_mut(&mut previous.entries);
    entries.extend_from_slice(added);
    previous.derived.extend(entries, added);
}

/// 有界、线外分页。空 tail 必须返回空，不能把 `max=None` 当成读到最新。
async fn scan(
    storage: &dyn Storage,
    conversation: ConversationId,
    min: Option<EntryId>,
    tail: Option<EntryId>,
) -> Result<Vec<EntryRecord>, DurableError> {
    let Some(tail) = tail else {
        return Ok(Vec::new());
    };
    let query = EntryQuery {
        min,
        max: Some(tail),
        order: Some(ScanOrder::Ascending),
        ..EntryQuery::new(conversation)
    };
    let mut cursor = None;
    let mut entries = Vec::new();
    loop {
        let page = storage.scan_entries(&query, PAGE, cursor.as_ref()).await?;
        entries.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests;
