//! 可编脚本的假 [`ZoneResolver`]。
//!
//! §10 有两条只在夏令时切换那两天才成立的规则——「同一日历时间因夏令时出现两次时，两
//! 次不同 UTC 时刻分别视为计划时间」和「不存在的本地时间跳过」。要测它们就得有一个会
//! 报告 [`ZoneResolution::Ambiguous`] 与 [`ZoneResolution::Gap`] 的区，而这两条恰恰是
//! [`FixedOffsetZone`](crate::cron::FixedOffsetZone) **答不出**的。

use time::{Duration, OffsetDateTime, PrimitiveDateTime, UtcOffset};

use crate::cron::{ZoneError, ZoneResolution};
use crate::traits::ZoneResolver;

/// 一个只有一次"向前拨"和一次"向后拨"的假时区。
///
/// 语义按真实的欧洲规则摆：
///
/// - `spring_forward` 是**第一个被跳掉的本地时间**。本地 `[sf, sf + shift)` 不存在。
/// - `fall_back` 是**第一个重复出现的本地时间**。本地 `[fb, fb + shift)` 各出现两次：
///   先以夏令时偏移，后以标准偏移。
///
/// 默认那套值就是欧洲 2026 年的切换日（+1 / +2，3 月 29 日与 10 月 25 日），拿它跑
/// `30 2 * * *` 能同时踩到两条规则。
#[derive(Debug, Clone, Copy)]
pub struct ScriptedZoneResolver {
    standard: UtcOffset,
    daylight: UtcOffset,
    spring_forward: PrimitiveDateTime,
    fall_back: PrimitiveDateTime,
}

impl ScriptedZoneResolver {
    pub fn new(
        standard: UtcOffset,
        daylight: UtcOffset,
        spring_forward: PrimitiveDateTime,
        fall_back: PrimitiveDateTime,
    ) -> Self {
        Self {
            standard,
            daylight,
            spring_forward,
            fall_back,
        }
    }

    /// 欧洲 2026：CET(+1) / CEST(+2)，3 月 29 日本地 02:00 跳到 03:00，10 月 25 日本地
    /// 03:00 退回 02:00。
    pub fn european_2026() -> Self {
        Self::new(
            time::macros::offset!(+1),
            time::macros::offset!(+2),
            time::macros::datetime!(2026-03-29 02:00:00),
            time::macros::datetime!(2026-10-25 02:00:00),
        )
    }

    fn shift(&self) -> Duration {
        Duration::seconds((self.daylight.whole_seconds() - self.standard.whole_seconds()) as i64)
    }

    /// 夏令时开始的那个瞬时。
    fn daylight_starts(&self) -> OffsetDateTime {
        self.spring_forward.assume_offset(self.standard)
    }

    /// 夏令时结束的那个瞬时。
    fn daylight_ends(&self) -> OffsetDateTime {
        self.fall_back.assume_offset(self.standard)
    }
}

impl ZoneResolver for ScriptedZoneResolver {
    fn resolve(&self, _zone: &str, local: PrimitiveDateTime) -> Result<ZoneResolution, ZoneError> {
        let shift = self.shift();
        let gap = self.spring_forward..(self.spring_forward + shift);
        let overlap = self.fall_back..(self.fall_back + shift);

        if gap.contains(&local) {
            return Ok(ZoneResolution::Gap);
        }
        if overlap.contains(&local) {
            // 先早后晚：时钟往回拨时，较早的那个瞬时用的是**更大**的偏移。
            return Ok(ZoneResolution::Ambiguous(self.daylight, self.standard));
        }
        let in_daylight = local >= self.spring_forward + shift && local < self.fall_back;
        Ok(ZoneResolution::Single(if in_daylight {
            self.daylight
        } else {
            self.standard
        }))
    }

    fn offset_at(&self, _zone: &str, instant: OffsetDateTime) -> Result<UtcOffset, ZoneError> {
        Ok(
            if instant >= self.daylight_starts() && instant < self.daylight_ends() {
                self.daylight
            } else {
                self.standard
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §10：「同一日历时间因夏令时出现两次时，两次不同 UTC 时刻分别视为计划时间」。
    #[test]
    fn a_repeated_local_time_fires_at_both_utc_instants() {
        use crate::cron::{TimeZone, Trigger, next_occurrence};

        let zones = ScriptedZoneResolver::european_2026();
        // 10 月 25 日本地 [02:00, 03:00) 出现两次，`30 2 * * *` 正落在里面。
        let trigger = Trigger::Cron {
            expr: "30 2 * * *".into(),
            tz: TimeZone::new("Europe/Berlin"),
        };

        let before = time::macros::datetime!(2026-10-24 12:00:00 UTC);
        let first = next_occurrence(&trigger, before, &zones).unwrap().unwrap();
        let second = next_occurrence(&trigger, first, &zones).unwrap().unwrap();

        // 第一次是夏令时里的 02:30（+2 → 00:30Z），第二次是退回之后的 02:30（+1 → 01:30Z）。
        assert_eq!(
            first.to_offset(time::UtcOffset::UTC),
            time::macros::datetime!(2026-10-25 00:30:00 UTC)
        );
        assert_eq!(
            second.to_offset(time::UtcOffset::UTC),
            time::macros::datetime!(2026-10-25 01:30:00 UTC)
        );
        assert_ne!(first, second, "两个不同的 UTC 时刻");
        assert_eq!(first.hour(), 2, "两次在墙上都是 02:30");
        assert_eq!(second.hour(), 2);
        assert_eq!(first.minute(), 30);
        assert_eq!(second.minute(), 30);
        assert_eq!(first.offset(), time::macros::offset!(+2));
        assert_eq!(second.offset(), time::macros::offset!(+1));
    }

    /// §10：「不存在的本地时间跳过」。
    #[test]
    fn a_local_time_that_does_not_exist_is_skipped() {
        use crate::cron::{TimeZone, Trigger, next_occurrence};

        let zones = ScriptedZoneResolver::european_2026();
        // 3 月 29 日本地 [02:00, 03:00) 被跳掉，`30 2 * * *` 那天没有计划时间。
        let trigger = Trigger::Cron {
            expr: "30 2 * * *".into(),
            tz: TimeZone::new("Europe/Berlin"),
        };

        let before = time::macros::datetime!(2026-03-28 12:00:00 UTC);
        let next = next_occurrence(&trigger, before, &zones).unwrap().unwrap();

        // 28 日的 02:30 已经过去了（before 是当天中午），29 日的不存在，所以落到 30 日。
        assert_eq!(
            next.to_offset(time::UtcOffset::UTC),
            time::macros::datetime!(2026-03-30 00:30:00 UTC)
        );
        assert_eq!(next.day(), 30, "29 日那一槽被跳过，不是挪到 03:30");
        assert_eq!(next.hour(), 2);
        assert_eq!(next.minute(), 30);
    }

    /// 一次性触发写到一个不存在的本地时间上——当场拒绝，趁打字的人还在。
    #[test]
    fn a_one_shot_at_a_nonexistent_local_time_is_refused() {
        use crate::cron::{ScheduleError, TimeZone, parse_schedule};

        let zones = ScriptedZoneResolver::european_2026();
        let err = parse_schedule(
            "@at 2026-03-29 02:30",
            &TimeZone::new("Europe/Berlin"),
            time::macros::datetime!(2026-03-01 00:00:00 UTC),
            &zones,
        )
        .unwrap_err();
        assert!(
            matches!(err, ScheduleError::LocalTimeDoesNotExist { .. }),
            "{err}"
        );
    }

    /// 落在重复区间里的一次性触发取较早的那个瞬时。
    #[test]
    fn a_one_shot_at_a_repeated_local_time_takes_the_earlier_instant() {
        use crate::cron::{TimeZone, Trigger, parse_schedule};

        let zones = ScriptedZoneResolver::european_2026();
        let trigger = parse_schedule(
            "@at 2026-10-25 02:30",
            &TimeZone::new("Europe/Berlin"),
            time::macros::datetime!(2026-10-01 00:00:00 UTC),
            &zones,
        )
        .unwrap();
        let Trigger::At { at } = trigger else {
            panic!()
        };
        assert_eq!(
            at.to_offset(time::UtcOffset::UTC),
            time::macros::datetime!(2026-10-25 00:30:00 UTC)
        );
    }

    /// 切换日之外的日子照常，一天一次。
    #[test]
    fn an_ordinary_day_still_fires_once() {
        use crate::cron::{TimeZone, Trigger, next_occurrence};

        let zones = ScriptedZoneResolver::european_2026();
        let trigger = Trigger::Cron {
            expr: "30 2 * * *".into(),
            tz: TimeZone::new("Europe/Berlin"),
        };
        let first = next_occurrence(
            &trigger,
            time::macros::datetime!(2026-07-01 12:00:00 UTC),
            &zones,
        )
        .unwrap()
        .unwrap();
        let second = next_occurrence(&trigger, first, &zones).unwrap().unwrap();
        assert_eq!(second - first, time::Duration::hours(24));
        assert_eq!(first.offset(), time::macros::offset!(+2), "七月是夏令时");
    }
}
