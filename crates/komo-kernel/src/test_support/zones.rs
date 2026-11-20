//! 可编脚本的假 [`ZoneResolver`]。
//!
//! §10 有两条只在夏令时切换那两天才成立的规则——「同一日历时间因夏令时出现两次时，两
//! 次不同 UTC 时刻分别视为计划时间」和「不存在的本地时间跳过」。要测它们就得有一个会
//! 报告 [`ZoneResolution::Ambiguous`] 与 [`ZoneResolution::Gap`] 的区，而这两条恰恰是
//! [`FixedOffsetZone`](crate::cron::FixedOffsetZone) **答不出**的。

use jiff::civil::DateTime;
use jiff::tz::Offset;
use jiff::{SignedDuration, Timestamp};

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
    standard: Offset,
    daylight: Offset,
    spring_forward: DateTime,
    fall_back: DateTime,
}

impl ScriptedZoneResolver {
    pub fn new(
        standard: Offset,
        daylight: Offset,
        spring_forward: DateTime,
        fall_back: DateTime,
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
            jiff::tz::offset(1),
            jiff::tz::offset(2),
            jiff::civil::date(2026, 3, 29).at(2, 0, 0, 0),
            jiff::civil::date(2026, 10, 25).at(2, 0, 0, 0),
        )
    }

    fn shift(&self) -> SignedDuration {
        SignedDuration::from_secs(i64::from(self.daylight.seconds() - self.standard.seconds()))
    }

    /// 夏令时开始的那个瞬时。
    fn daylight_starts(&self) -> Timestamp {
        self.standard
            .to_timestamp(self.spring_forward)
            .expect("脚本里的切换时刻在范围内")
    }

    /// 夏令时结束的那个瞬时。
    fn daylight_ends(&self) -> Timestamp {
        self.standard
            .to_timestamp(self.fall_back)
            .expect("脚本里的切换时刻在范围内")
    }
}

impl ZoneResolver for ScriptedZoneResolver {
    fn resolve(&self, _zone: &str, local: DateTime) -> Result<ZoneResolution, ZoneError> {
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

    fn offset_at(&self, _zone: &str, instant: Timestamp) -> Result<Offset, ZoneError> {
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

    /// 这个瞬时在脚本时区里的墙上时间与偏移。
    fn wall(zones: &ScriptedZoneResolver, at: Timestamp) -> (DateTime, Offset) {
        let offset = zones.offset_at("Europe/Berlin", at).unwrap();
        (offset.to_datetime(at), offset)
    }

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

        let before = "2026-10-24T12:00:00Z".parse::<jiff::Timestamp>().unwrap();
        let first = next_occurrence(&trigger, before, &zones).unwrap().unwrap();
        let second = next_occurrence(&trigger, first, &zones).unwrap().unwrap();

        // 第一次是夏令时里的 02:30（+2 → 00:30Z），第二次是退回之后的 02:30（+1 → 01:30Z）。
        assert_eq!(
            first,
            "2026-10-25T00:30:00Z".parse::<jiff::Timestamp>().unwrap()
        );
        assert_eq!(
            second,
            "2026-10-25T01:30:00Z".parse::<jiff::Timestamp>().unwrap()
        );
        assert_ne!(first, second, "两个不同的 UTC 时刻");
        let (first_wall, first_offset) = wall(&zones, first);
        let (second_wall, second_offset) = wall(&zones, second);
        assert_eq!(first_wall.hour(), 2, "两次在墙上都是 02:30");
        assert_eq!(second_wall.hour(), 2);
        assert_eq!(first_wall.minute(), 30);
        assert_eq!(second_wall.minute(), 30);
        assert_eq!(first_offset, jiff::tz::offset(2));
        assert_eq!(second_offset, jiff::tz::offset(1));
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

        let before = "2026-03-28T12:00:00Z".parse::<jiff::Timestamp>().unwrap();
        let next = next_occurrence(&trigger, before, &zones).unwrap().unwrap();

        // 28 日的 02:30 已经过去了（before 是当天中午），29 日的不存在，所以落到 30 日。
        assert_eq!(
            next,
            "2026-03-30T00:30:00Z".parse::<jiff::Timestamp>().unwrap()
        );
        let (next_wall, _) = wall(&zones, next);
        assert_eq!(next_wall.day(), 30, "29 日那一槽被跳过，不是挪到 03:30");
        assert_eq!(next_wall.hour(), 2);
        assert_eq!(next_wall.minute(), 30);
    }

    /// 一次性触发写到一个不存在的本地时间上——当场拒绝，趁打字的人还在。
    #[test]
    fn a_one_shot_at_a_nonexistent_local_time_is_refused() {
        use crate::cron::{ScheduleError, TimeZone, parse_schedule};

        let zones = ScriptedZoneResolver::european_2026();
        let err = parse_schedule(
            "@at 2026-03-29 02:30",
            &TimeZone::new("Europe/Berlin"),
            "2026-03-01T00:00:00Z".parse::<jiff::Timestamp>().unwrap(),
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
            "2026-10-01T00:00:00Z".parse::<jiff::Timestamp>().unwrap(),
            &zones,
        )
        .unwrap();
        let Trigger::At { at } = trigger else {
            panic!()
        };
        assert_eq!(
            at,
            "2026-10-25T00:30:00Z".parse::<jiff::Timestamp>().unwrap()
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
            "2026-07-01T12:00:00Z".parse::<jiff::Timestamp>().unwrap(),
            &zones,
        )
        .unwrap()
        .unwrap();
        let second = next_occurrence(&trigger, first, &zones).unwrap().unwrap();
        assert_eq!(second - first, jiff::SignedDuration::from_hours(24));
        assert_eq!(wall(&zones, first).1, jiff::tz::offset(2), "七月是夏令时");
    }
}
