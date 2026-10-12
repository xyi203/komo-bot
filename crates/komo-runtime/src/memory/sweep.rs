//! 按规则遗忘（§9.6 之后补的一步）。
//!
//! 抽取只会往库里加东西，没有东西去拿掉不再有用的那些，库会一直长，召回里的噪声也跟着
//! 长。这里是确定性的那一半：过期的、入库很久从没用过的、很久没再用的，遗忘；正文提到
//! 的路径已经不在的，标 contested 暂停召回，留给人或模型判断（路径可能只是挪了）。
//! 重复、过时、一次性流水这类要读懂正文才判得了的，不在这里——那是定时任务里模型的事。
//!
//! **已确认的记忆不在任何规则的射程里。** 判定是纯函数 [`plan`]，无数据库可测；取库存
//! 与落库在 [`super::MemoryManager::sweep`]。

use std::path::Path;

use komo_kernel::protocol::http::{SweepAction, SweepRule};
use komo_kernel::types::memory::{Confirmation, MemoryItem, MemoryState};
use time::{Duration, OffsetDateTime};

use super::stale::missing_paths;

/// 两个时间阈值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepPolicy {
    /// 入库超过这么久、一次都没被用过的，遗忘。
    pub never_used: Duration,
    /// 最近这么久没被用过的，遗忘。
    pub idle: Duration,
}

impl SweepPolicy {
    pub fn from_days(never_used: u32, idle: u32) -> Self {
        SweepPolicy {
            never_used: Duration::days(i64::from(never_used)),
            idle: Duration::days(i64::from(idle)),
        }
    }
}

/// 对每条记忆给出处理意见；不处理的不出现在结果里。
pub fn plan(
    items: &[MemoryItem],
    now: OffsetDateTime,
    policy: SweepPolicy,
    home: Option<&Path>,
) -> Vec<SweepAction> {
    items
        .iter()
        .filter_map(|item| judge(item, now, policy, home))
        .collect()
}

fn judge(
    item: &MemoryItem,
    now: OffsetDateTime,
    policy: SweepPolicy,
    home: Option<&Path>,
) -> Option<SweepAction> {
    if item.confirmation == Confirmation::UserConfirmed
        || !matches!(item.state, MemoryState::Active | MemoryState::Candidate)
    {
        return None;
    }
    let forget = |rule, detail| Some(action(item, rule, MemoryState::Forgotten, detail));

    if let Some(until) = item.valid_until
        && until <= now
    {
        return forget(
            SweepRule::Expired,
            format!("有效期到 {} 为止", until.date()),
        );
    }
    match item.usage.last_used_at {
        None if now - item.created_at >= policy.never_used => {
            return forget(
                SweepRule::NeverUsed,
                format!(
                    "入库 {} 天，从没被用过",
                    (now - item.created_at).whole_days()
                ),
            );
        }
        Some(last) if now - last >= policy.idle => {
            return forget(
                SweepRule::Idle,
                format!(
                    "{} 天没被用过（之前用过 {} 次）",
                    (now - last).whole_days(),
                    item.usage.count
                ),
            );
        }
        _ => {}
    }
    // 候选本来就不进召回，暂停召回对它没有意义。
    if item.state == MemoryState::Active {
        let missing = missing_paths(&item.content, home);
        if !missing.is_empty() {
            return Some(action(
                item,
                SweepRule::MissingPath,
                MemoryState::Contested,
                format!("{} 已不存在", missing.join("、")),
            ));
        }
    }
    None
}

fn action(item: &MemoryItem, rule: SweepRule, to: MemoryState, detail: String) -> SweepAction {
    SweepAction {
        memory: item.id.clone(),
        revision: item.revision,
        rule,
        to,
        content: item.content.clone(),
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::types::ids::MemoryId;
    use komo_kernel::types::memory::{
        ExtractionMetadata, MemoryKind, MemoryScope, MemoryUsage, Provenance,
    };

    const DAY: Duration = Duration::days(1);

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(1000)
    }

    fn item(content: &str, age: Duration, last_used: Option<Duration>) -> MemoryItem {
        MemoryItem {
            id: MemoryId::from_raw(content),
            revision: 2,
            content: content.into(),
            kind: MemoryKind::Fact,
            scope: MemoryScope::Personal,
            provenance: Provenance::ToolObservation,
            confirmation: Confirmation::Unconfirmed,
            state: MemoryState::Active,
            evidence: vec![],
            observed_at: now() - age,
            valid_until: None,
            created_at: now() - age,
            updated_at: now() - age,
            extraction: ExtractionMetadata::new("memory-model", None, "v1"),
            usage: MemoryUsage {
                count: u64::from(last_used.is_some()) * 3,
                last_used_at: last_used.map(|ago| now() - ago),
            },
            supersedes: None,
        }
    }

    fn rules(items: &[MemoryItem], home: Option<&Path>) -> Vec<(String, SweepRule, MemoryState)> {
        plan(items, now(), SweepPolicy::from_days(60, 90), home)
            .into_iter()
            .map(|a| (a.memory.to_string(), a.rule, a.to))
            .collect()
    }

    #[test]
    fn old_unused_idle_and_expired_memories_are_forgotten() {
        let mut expired = item("expired", DAY, Some(DAY));
        expired.valid_until = Some(now() - DAY);
        let items = vec![
            item("fresh-unused", 59 * DAY, None),
            item("old-unused", 60 * DAY, None),
            item("recently-used", 400 * DAY, Some(89 * DAY)),
            item("idle", 400 * DAY, Some(90 * DAY)),
            expired,
        ];
        assert_eq!(
            rules(&items, None),
            vec![
                (
                    "old-unused".into(),
                    SweepRule::NeverUsed,
                    MemoryState::Forgotten
                ),
                ("idle".into(), SweepRule::Idle, MemoryState::Forgotten),
                ("expired".into(), SweepRule::Expired, MemoryState::Forgotten),
            ]
        );
    }

    #[test]
    fn confirmed_and_inactive_memories_are_never_touched() {
        let mut confirmed = item("confirmed", 400 * DAY, None);
        confirmed.confirmation = Confirmation::UserConfirmed;
        let mut contested = item("contested", 400 * DAY, None);
        contested.state = MemoryState::Contested;
        let mut forgotten = item("forgotten", 400 * DAY, None);
        forgotten.state = MemoryState::Forgotten;
        assert!(rules(&[confirmed, contested, forgotten], None).is_empty());
    }

    #[test]
    fn a_missing_path_contests_an_active_memory_but_not_a_candidate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".komo/toolbox")).unwrap();
        let mut candidate = item("候选在 ~/.komo/skills/gone", DAY, None);
        candidate.state = MemoryState::Candidate;
        let items = vec![
            item("能力在 ~/.komo/toolbox", DAY, Some(DAY)),
            item("技能在 ~/.komo/skills/gone", DAY, Some(DAY)),
            candidate,
        ];
        let actions = plan(
            &items,
            now(),
            SweepPolicy::from_days(60, 90),
            Some(dir.path()),
        );
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].memory.as_str(), "技能在 ~/.komo/skills/gone");
        assert_eq!(actions[0].rule, SweepRule::MissingPath);
        assert_eq!(actions[0].to, MemoryState::Contested);
        assert_eq!(actions[0].revision, 2);
        assert_eq!(actions[0].detail, "~/.komo/skills/gone 已不存在");
    }
}
