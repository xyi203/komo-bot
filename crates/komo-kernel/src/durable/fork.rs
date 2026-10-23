//! fork 遍历（pi-durable spec §2.1、§3.7；设计 §4.2）：纯函数，入参是记录，不读存储。
//!
//! 一个 conversation 的可见历史 = 自己的 entry，加上沿 `parent` 链往上、每个父会话
//! 被 `parent.at` 截断的 entry。祖先上限沿链取最小：孙会话看到的祖父 entry 同时受
//! 父会话的 fork 点和祖父自己的 fork 点约束。

use std::collections::HashSet;

use super::ids::{ConversationId, EntryId};
use super::records::{ConversationRecord, EntryRecord};
use super::storage::ScanOrder;

/// fork 链上的一段：某个 conversation 的 entry 到 `upper`（含）为止可见。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForkSegment {
    pub conversation: ConversationId,
    /// 可见上限（含）；`None` 表示没有上限。
    pub upper: Option<EntryId>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ForkError {
    #[error("unknown conversation: {0}")]
    UnknownConversation(ConversationId),
    #[error("conversation {0} forks itself through its ancestors")]
    Cycle(ConversationId),
}

/// 从 `start` 起沿 `parent` 往上的链，子在前；`min` / `max` 是调用方要的 entry 范围（含）。
///
/// 上限在每一步取 `min(自己的上限, parent.at)`；一旦上限低于 `min`，再往上的祖先
/// 不可能贡献任何 entry，链在这里截断。
pub fn fork_chain<'a>(
    start: ConversationId,
    lookup: impl Fn(ConversationId) -> Option<&'a ConversationRecord>,
    min: Option<EntryId>,
    max: Option<EntryId>,
) -> Result<Vec<ForkSegment>, ForkError> {
    let mut segments = Vec::new();
    let mut seen = HashSet::new();
    let mut current = start;
    let mut upper = max;
    loop {
        if !seen.insert(current) {
            return Err(ForkError::Cycle(current));
        }
        let record = lookup(current).ok_or(ForkError::UnknownConversation(current))?;
        segments.push(ForkSegment {
            conversation: current,
            upper,
        });
        let Some(parent) = record.parent else { break };
        let capped = upper.map_or(parent.at, |upper| upper.min(parent.at));
        if min.is_some_and(|min| capped < min) {
            break;
        }
        upper = Some(capped);
        current = parent.conversation;
    }
    Ok(segments)
}

/// 链上各段的可见 entry，按 `order` 排列。
///
/// `entries_of` 给出某个 conversation 自己的全部 entry，**按 ID 升序**。降序时先读最子
/// 的一段再往祖先走；升序时先读根一段，再读各个 fork 的。
pub fn visible_entries<'a>(
    chain: &[ForkSegment],
    entries_of: impl Fn(ConversationId) -> &'a [EntryRecord],
    min: Option<EntryId>,
    max: Option<EntryId>,
    order: ScanOrder,
) -> Vec<&'a EntryRecord> {
    let mut visible = Vec::new();
    let mut collect = |segment: &ForkSegment| {
        let upper = match (segment.upper, max) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let entries = entries_of(segment.conversation);
        let start = min.map_or(0, |min| entries.partition_point(|entry| entry.id < min));
        let end = upper.map_or(entries.len(), |upper| {
            entries.partition_point(|entry| entry.id <= upper)
        });
        if start < end {
            let range = &entries[start..end];
            match order {
                ScanOrder::Ascending => visible.extend(range),
                ScanOrder::Descending => visible.extend(range.iter().rev()),
            }
        }
    };
    match order {
        ScanOrder::Descending => chain.iter().for_each(&mut collect),
        ScanOrder::Ascending => chain.iter().rev().for_each(&mut collect),
    }
    visible
}

/// `entry`（属于它自己的 conversation）能否从 `chain` 的起点会话看到。
pub fn is_visible(chain: &[ForkSegment], conversation: ConversationId, entry: EntryId) -> bool {
    chain
        .iter()
        .find(|segment| segment.conversation == conversation)
        .is_some_and(|segment| segment.upper.is_none_or(|upper| entry <= upper))
}

/// 链上最新的、带 `head` 的可见 entry（`at_or_before` 为含的上限）。
pub fn find_latest_head_marker<'a>(
    chain: &[ForkSegment],
    entries_of: impl Fn(ConversationId) -> &'a [EntryRecord],
    at_or_before: Option<EntryId>,
) -> Option<&'a EntryRecord> {
    chain.iter().find_map(|segment| {
        let upper = match (segment.upper, at_or_before) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        entries_of(segment.conversation)
            .iter()
            .rev()
            .filter(|entry| upper.is_none_or(|upper| entry.id <= upper))
            .find(|entry| entry.head.is_some())
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn conv(id: u64) -> ConversationId {
        ConversationId::new(id)
    }

    fn eid(id: u64) -> EntryId {
        EntryId::new(id)
    }

    fn entry(id: u64, conversation: u64) -> EntryRecord {
        EntryRecord::new(eid(id), conv(conversation), "note")
    }

    /// 根 1：entry 10、20、30；2 在 20 处 fork：entry 40、50；3 在 2 的 40 处 fork：entry 60。
    struct Tree {
        conversations: HashMap<ConversationId, ConversationRecord>,
        entries: HashMap<ConversationId, Vec<EntryRecord>>,
    }

    fn tree() -> Tree {
        let conversations = [
            ConversationRecord::root(conv(1)),
            ConversationRecord::fork(conv(2), conv(1), eid(20)),
            ConversationRecord::fork(conv(3), conv(2), eid(40)),
        ]
        .into_iter()
        .map(|record| (record.id, record))
        .collect();
        let entries = HashMap::from([
            (conv(1), vec![entry(10, 1), entry(20, 1), entry(30, 1)]),
            (conv(2), vec![entry(40, 2), entry(50, 2)]),
            (conv(3), vec![entry(60, 3)]),
        ]);
        Tree {
            conversations,
            entries,
        }
    }

    impl Tree {
        fn ids(&self, from: u64, min: Option<u64>, max: Option<u64>, order: ScanOrder) -> Vec<u64> {
            let (min, max) = (min.map(eid), max.map(eid));
            let chain = fork_chain(conv(from), |id| self.conversations.get(&id), min, max).unwrap();
            visible_entries(
                &chain,
                |id| self.entries.get(&id).map_or(&[][..], Vec::as_slice),
                min,
                max,
                order,
            )
            .into_iter()
            .map(|entry| entry.id.get())
            .collect()
        }
    }

    #[test]
    fn child_sees_parent_entries_only_up_to_the_fork_point() {
        let tree = tree();
        assert_eq!(
            tree.ids(2, None, None, ScanOrder::Ascending),
            [10, 20, 40, 50]
        );
        assert_eq!(
            tree.ids(2, None, None, ScanOrder::Descending),
            [50, 40, 20, 10]
        );
        // 根自己不受影响：fork 点之后的 30 仍属于根。
        assert_eq!(tree.ids(1, None, None, ScanOrder::Ascending), [10, 20, 30]);
    }

    #[test]
    fn caps_compose_through_every_ancestor() {
        let tree = tree();
        assert_eq!(
            tree.ids(3, None, None, ScanOrder::Ascending),
            [10, 20, 40, 60]
        );
        assert_eq!(
            tree.ids(3, None, None, ScanOrder::Descending),
            [60, 40, 20, 10]
        );
    }

    #[test]
    fn bounds_trim_the_range_and_cut_the_chain_short() {
        let tree = tree();
        assert_eq!(
            tree.ids(3, Some(20), Some(40), ScanOrder::Ascending),
            [20, 40]
        );
        // min 高于祖先的上限：根一段不可能贡献，链在 conv 2 处断开。
        let chain = fork_chain(
            conv(3),
            |id| tree.conversations.get(&id),
            Some(eid(40)),
            None,
        )
        .unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(tree.ids(3, Some(40), None, ScanOrder::Ascending), [40, 60]);
    }

    #[test]
    fn unknown_conversations_and_cycles_are_reported() {
        let tree = tree();
        assert_eq!(
            fork_chain(conv(9), |id| tree.conversations.get(&id), None, None),
            Err(ForkError::UnknownConversation(conv(9)))
        );
        let looped: HashMap<_, _> = [
            ConversationRecord::fork(conv(1), conv(2), eid(5)),
            ConversationRecord::fork(conv(2), conv(1), eid(5)),
        ]
        .into_iter()
        .map(|record| (record.id, record))
        .collect();
        assert!(matches!(
            fork_chain(conv(1), |id| looped.get(&id), None, None),
            Err(ForkError::Cycle(_))
        ));
    }

    #[test]
    fn visibility_respects_each_segment_cap() {
        let tree = tree();
        let chain = fork_chain(conv(3), |id| tree.conversations.get(&id), None, None).unwrap();
        assert!(is_visible(&chain, conv(1), eid(20)));
        assert!(!is_visible(&chain, conv(1), eid(30)));
        assert!(is_visible(&chain, conv(2), eid(40)));
        assert!(!is_visible(&chain, conv(2), eid(50)));
        assert!(is_visible(&chain, conv(3), eid(60)));
    }

    #[test]
    fn head_marker_is_found_through_ancestors_and_respects_the_cutoff() {
        let mut tree = tree();
        tree.entries.get_mut(&conv(1)).unwrap()[1].head = Some(eid(10));
        tree.entries.get_mut(&conv(1)).unwrap()[2].head = Some(eid(30));
        let find = |from: u64, at: Option<u64>| {
            let chain = fork_chain(
                conv(from),
                |id| tree.conversations.get(&id),
                None,
                at.map(eid),
            )
            .unwrap();
            find_latest_head_marker(
                &chain,
                |id| tree.entries.get(&id).map_or(&[][..], Vec::as_slice),
                at.map(eid),
            )
            .map(|entry| entry.id.get())
        };
        // 30 在 conv 2 的 fork 点（20）之后，对子会话不可见。
        assert_eq!(find(2, None), Some(20));
        assert_eq!(find(1, None), Some(30));
        assert_eq!(find(1, Some(25)), Some(20));
        assert_eq!(find(1, Some(15)), None);
    }
}
