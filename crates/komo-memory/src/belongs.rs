//! 记忆的归属与结案归档（`docs/memory.md` §9.2「归属与归档」、§9.6「结案归档」，
//! `docs/goals.md` §6）。
//!
//! 结案与归档**不在同一个 aux.db 事务里**：事项仓储与记忆仓储是两个 trait，这里先让事项写
//! 完（调用方），再跑这一遍归档。它是幂等的（只动还没归档的），所以失败后重发同一个结案
//! 请求就能补上；补上之前，[`komo_kernel::types::memory::clue_is_pushed`] 已经不再推送
//! 已结案事项名下的线索，不会出现"事结了、线索还在推"。

use komo_kernel::types::ids::MatterId;
use komo_kernel::types::memory::{MemoryItem, MemoryState};

use super::{MemoryError, MemoryManager};

/// 一次结案或重开对这件事名下的记忆做了什么。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BelongsOutcome {
    /// 转成 `archived` 的线索条数。
    pub archived: u32,
    /// 从 `archived` 恢复成 `active` 的条数。
    pub restored: u32,
    /// 归属它、仍在生效的约定（不动，交给操作者定去向）。
    pub conventions: Vec<MemoryItem>,
}

/// 结案时这条要不要归档：线索（reference 或还没分类的）且 active / candidate。contested 等人裁，
/// 不因结案消失；约定是用户的长期意图，不动（§9.6）。
fn archives_on_close(item: &MemoryItem) -> bool {
    !item.is_convention() && matches!(item.state, MemoryState::Active | MemoryState::Candidate)
}

impl MemoryManager {
    /// 事项结案（done / drop / retire）：归属它的线索归档，列出它名下仍在生效的约定。
    pub async fn archive_for(&self, matter: &MatterId) -> Result<BelongsOutcome, MemoryError> {
        let now = self.clock.now();
        let mut out = BelongsOutcome::default();
        for item in self.catalog.belonging_to(matter).await? {
            if item.is_convention() && item.state == MemoryState::Active {
                out.conventions.push(item);
                continue;
            }
            if !archives_on_close(&item) {
                continue;
            }
            let expected = item.revision;
            let mut next = item;
            next.state = MemoryState::Archived;
            next.revision = expected + 1;
            next.updated_at = now;
            self.repo.put(next, Some(expected)).await?;
            out.archived += 1;
        }
        Ok(out)
    }

    /// 事项重开：归属它的 `archived` 恢复成 `active`。只有结案会产生 `archived`，所以"归属它的
    /// archived"就是因它结案而归档的那些。
    pub async fn restore_for(&self, matter: &MatterId) -> Result<BelongsOutcome, MemoryError> {
        let now = self.clock.now();
        let mut out = BelongsOutcome::default();
        for item in self.catalog.belonging_to(matter).await? {
            if item.state != MemoryState::Archived {
                continue;
            }
            let expected = item.revision;
            let mut next = item;
            next.state = MemoryState::Active;
            next.revision = expected + 1;
            next.updated_at = now;
            self.repo.put(next, Some(expected)).await?;
            out.restored += 1;
        }
        Ok(out)
    }
}
