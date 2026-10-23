//! 任务定义的版本匹配（spec §5.4 的表）：缺定义、版本太老、需要迁移、迁移失败。

/// 定义里与匹配有关的部分；调度器从注册表的定义取出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefinitionInfo {
    pub version: u32,
    pub has_migrate: bool,
}

/// 为什么一条 pending 任务当前不能被 reserve。派生状态，从不落盘。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockedReason {
    MissingTask,
    TaskTooOld,
    MigrationFailed,
}

impl BlockedReason {
    /// 落在 `orphaned` 的 reason 上的字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            BlockedReason::MissingTask => "missing_task",
            BlockedReason::TaskTooOld => "task_too_old",
            BlockedReason::MigrationFailed => "migration_failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// 版本相同，直接 `running`；或更旧的记录，要先 migrate。
    Ready {
        migrates: bool,
    },
    Blocked(BlockedReason),
}

/// 一条记录（版本 `stored`）遇到定义 `definition`：
///
/// ```text
/// 缺定义                -> missing_task
/// 存的版本比定义新      -> task_too_old
/// 存的版本比定义旧      -> migrate（没有 migrate 回调按迁移失败算；这个定义试过且失败
///                          过则保持阻塞，直到注册表换一个定义对象）
/// 相同                  -> 直接 running
/// ```
///
/// `migration_failed_with_this_definition`：这条任务已经被**这个定义对象**迁移失败过。
pub fn fit(
    stored: u32,
    definition: Option<DefinitionInfo>,
    migration_failed_with_this_definition: bool,
) -> Fit {
    let Some(definition) = definition else {
        return Fit::Blocked(BlockedReason::MissingTask);
    };
    match definition.version.cmp(&stored) {
        std::cmp::Ordering::Equal => Fit::Ready { migrates: false },
        std::cmp::Ordering::Less => Fit::Blocked(BlockedReason::TaskTooOld),
        std::cmp::Ordering::Greater => {
            if migration_failed_with_this_definition || !definition.has_migrate {
                Fit::Blocked(BlockedReason::MigrationFailed)
            } else {
                Fit::Ready { migrates: true }
            }
        }
    }
}

/// 阶段边界换定义时：新定义能不能接手（同版本，或更新且有 migrate）。
pub fn can_reserve(stored: u32, definition: DefinitionInfo) -> bool {
    definition.version == stored || (definition.version > stored && definition.has_migrate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(version: u32, has_migrate: bool) -> Option<DefinitionInfo> {
        Some(DefinitionInfo {
            version,
            has_migrate,
        })
    }

    #[test]
    fn the_table_of_spec_5_4() {
        assert_eq!(
            fit(1, None, false),
            Fit::Blocked(BlockedReason::MissingTask)
        );
        assert_eq!(
            fit(2, def(1, true), false),
            Fit::Blocked(BlockedReason::TaskTooOld)
        );
        assert_eq!(fit(1, def(1, false), false), Fit::Ready { migrates: false });
        assert_eq!(fit(1, def(2, true), false), Fit::Ready { migrates: true });
    }

    #[test]
    fn a_newer_definition_without_migrate_or_after_a_failed_migration_blocks() {
        assert_eq!(
            fit(1, def(2, false), false),
            Fit::Blocked(BlockedReason::MigrationFailed)
        );
        assert_eq!(
            fit(1, def(2, true), true),
            Fit::Blocked(BlockedReason::MigrationFailed)
        );
    }

    #[test]
    fn handover_needs_the_same_version_or_a_newer_one_with_migrate() {
        let d = |v, m| DefinitionInfo {
            version: v,
            has_migrate: m,
        };
        assert!(can_reserve(1, d(1, false)));
        assert!(can_reserve(1, d(2, true)));
        assert!(!can_reserve(1, d(2, false)));
        assert!(!can_reserve(2, d(1, true)));
    }

    #[test]
    fn orphan_reasons_are_the_blocked_reason_names() {
        assert_eq!(BlockedReason::MissingTask.as_str(), "missing_task");
        assert_eq!(BlockedReason::TaskTooOld.as_str(), "task_too_old");
        assert_eq!(BlockedReason::MigrationFailed.as_str(), "migration_failed");
    }
}
