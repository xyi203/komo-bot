//! 真 tzdb 的 [`ZoneResolver`]（§10、§13.5）。
//!
//! kernel 不带 tzdb——它是一份随操作系统更新的数据，属于运行环境而不是领域——所以
//! 「Europe/Berlin 在 2026-03-29 02:30 是几点」这个问题由这里回答。jiff 自带一份
//! tzdb（系统有 `/usr/share/zoneinfo` 就用系统的），于是这个文件只做两件事：
//! **名字 → 规则**，以及 `time` 与 jiff 之间按 **unix 秒 + 纳秒** 换算。
//!
//! 两个方向都要（§13.5）：`resolve` 是本地民用时间 → 瞬时（可能零个、一个或两个），
//! `offset_at` 是瞬时 → 本地偏移。§10 的两条夏令时规则由 kernel 的 `cron` 实现，这里
//! 只负责**把三种情形如实报上去**——把 `Fold` 报成 `Single` 就等于当这个区没有夏令时。

use komo_kernel::cron::{ZoneError, ZoneResolution};
use komo_kernel::traits::ZoneResolver;
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};

/// 用 jiff 的 tzdb 回答时区问题。
///
/// 无状态（jiff 自己缓存已解析的时区），所以随便克隆、随便共享。
#[derive(Debug, Clone, Copy, Default)]
pub struct JiffZoneResolver;

impl JiffZoneResolver {
    pub fn new() -> Self {
        JiffZoneResolver
    }

    fn zone(name: &str) -> Result<jiff::tz::TimeZone, ZoneError> {
        jiff::tz::TimeZone::get(name).map_err(|_| ZoneError::UnknownZone(name.to_string()))
    }
}

impl ZoneResolver for JiffZoneResolver {
    fn resolve(&self, zone: &str, local: PrimitiveDateTime) -> Result<ZoneResolution, ZoneError> {
        use jiff::tz::AmbiguousOffset;

        let tz = Self::zone(zone)?;
        let civil = to_civil(local)?;
        Ok(match tz.to_ambiguous_timestamp(civil).offset() {
            AmbiguousOffset::Unambiguous { offset } => {
                ZoneResolution::Single(to_utc_offset(offset)?)
            }
            // 时钟往回拨：`before` 是较早那个瞬时的偏移（更大的那个），`after` 是较晚
            // 的——正是 kernel 要求的"先早后晚"。
            AmbiguousOffset::Fold { before, after } => {
                ZoneResolution::Ambiguous(to_utc_offset(before)?, to_utc_offset(after)?)
            }
            // 时钟往前拨：这个本地时间不存在。§10 说跳过，所以这里如实报 Gap，不替
            // 调用方挑一个"最接近的"瞬时。
            AmbiguousOffset::Gap { .. } => ZoneResolution::Gap,
        })
    }

    fn offset_at(&self, zone: &str, instant: OffsetDateTime) -> Result<UtcOffset, ZoneError> {
        let tz = Self::zone(zone)?;
        to_utc_offset(tz.to_offset(to_timestamp(instant)?))
    }
}

fn to_civil(local: PrimitiveDateTime) -> Result<jiff::civil::DateTime, ZoneError> {
    jiff::civil::DateTime::new(
        local.year() as i16,
        local.month() as i8,
        local.day() as i8,
        local.hour() as i8,
        local.minute() as i8,
        local.second() as i8,
        local.nanosecond() as i32,
    )
    .map_err(|e| ZoneError::Unavailable(format!("{local} 不是一个可表示的民用时间：{e}")))
}

/// `time` → jiff，按 unix 秒 + 纳秒。
fn to_timestamp(instant: OffsetDateTime) -> Result<jiff::Timestamp, ZoneError> {
    jiff::Timestamp::new(instant.unix_timestamp(), instant.nanosecond() as i32)
        .map_err(|e| ZoneError::Unavailable(format!("{instant} 超出可表示范围：{e}")))
}

fn to_utc_offset(offset: jiff::tz::Offset) -> Result<UtcOffset, ZoneError> {
    UtcOffset::from_whole_seconds(offset.seconds())
        .map_err(|e| ZoneError::Unavailable(format!("偏移 {offset} 表示不了：{e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::cron::{ScheduleError, TimeZone, Trigger, next_occurrence, parse_schedule};
    use time::macros::{datetime, offset};

    /// 这五条和 kernel `test_support::zones` 里那五条是**同一组断言**，只是那边跑的是
    /// 可编脚本的假时区，这里跑的是真 tzdb 的 Europe/Berlin。假的答对了而真的答错，
    /// 说明这个 resolver 有问题；反过来说明 kernel 的规则实现有问题。
    fn berlin() -> TimeZone {
        TimeZone::new("Europe/Berlin")
    }

    fn daily_at_0230() -> Trigger {
        Trigger::Cron {
            expr: "30 2 * * *".into(),
            tz: berlin(),
        }
    }

    /// §10：「同一日历时间因夏令时出现两次时，两次不同 UTC 时刻分别视为计划时间」。
    #[test]
    fn a_repeated_local_time_fires_at_both_utc_instants() {
        let zones = JiffZoneResolver::new();
        let trigger = daily_at_0230();

        let before = datetime!(2026-10-24 12:00:00 UTC);
        let first = next_occurrence(&trigger, before, &zones).unwrap().unwrap();
        let second = next_occurrence(&trigger, first, &zones).unwrap().unwrap();

        assert_eq!(
            first.to_offset(UtcOffset::UTC),
            datetime!(2026-10-25 00:30:00 UTC)
        );
        assert_eq!(
            second.to_offset(UtcOffset::UTC),
            datetime!(2026-10-25 01:30:00 UTC)
        );
        assert_ne!(first, second, "两个不同的 UTC 时刻");
        assert_eq!(first.hour(), 2, "两次在墙上都是 02:30");
        assert_eq!(second.hour(), 2);
        assert_eq!(first.minute(), 30);
        assert_eq!(second.minute(), 30);
        assert_eq!(first.offset(), offset!(+2));
        assert_eq!(second.offset(), offset!(+1));
    }

    /// §10：「不存在的本地时间跳过」。
    #[test]
    fn a_local_time_that_does_not_exist_is_skipped() {
        let zones = JiffZoneResolver::new();
        let next = next_occurrence(&daily_at_0230(), datetime!(2026-03-28 12:00:00 UTC), &zones)
            .unwrap()
            .unwrap();

        assert_eq!(
            next.to_offset(UtcOffset::UTC),
            datetime!(2026-03-30 00:30:00 UTC)
        );
        assert_eq!(next.day(), 30, "29 日那一槽被跳过，不是挪到 03:30");
        assert_eq!(next.hour(), 2);
        assert_eq!(next.minute(), 30);
    }

    /// 一次性触发写到一个不存在的本地时间上——当场拒绝，趁打字的人还在。
    #[test]
    fn a_one_shot_at_a_nonexistent_local_time_is_refused() {
        let zones = JiffZoneResolver::new();
        let err = parse_schedule(
            "@at 2026-03-29 02:30",
            &berlin(),
            datetime!(2026-03-01 00:00:00 UTC),
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
        let zones = JiffZoneResolver::new();
        let trigger = parse_schedule(
            "@at 2026-10-25 02:30",
            &berlin(),
            datetime!(2026-10-01 00:00:00 UTC),
            &zones,
        )
        .unwrap();
        let Trigger::At { at } = trigger else {
            panic!("@at 解析成了别的东西")
        };
        assert_eq!(
            at.to_offset(UtcOffset::UTC),
            datetime!(2026-10-25 00:30:00 UTC)
        );
    }

    /// 切换日之外的日子照常，一天一次。
    #[test]
    fn an_ordinary_day_still_fires_once() {
        let zones = JiffZoneResolver::new();
        let first = next_occurrence(&daily_at_0230(), datetime!(2026-07-01 12:00:00 UTC), &zones)
            .unwrap()
            .unwrap();
        let second = next_occurrence(&daily_at_0230(), first, &zones)
            .unwrap()
            .unwrap();
        assert_eq!(second - first, time::Duration::hours(24));
        assert_eq!(first.offset(), offset!(+2), "七月是夏令时");
    }

    // ---- resolver 自己的两个方向 ----

    #[test]
    fn the_three_resolutions_are_reported_as_they_are() {
        let zones = JiffZoneResolver::new();
        assert_eq!(
            zones
                .resolve("Europe/Berlin", datetime!(2026-03-29 02:30:00))
                .unwrap(),
            ZoneResolution::Gap
        );
        assert_eq!(
            zones
                .resolve("Europe/Berlin", datetime!(2026-10-25 02:30:00))
                .unwrap(),
            ZoneResolution::Ambiguous(offset!(+2), offset!(+1)),
            "先早后晚"
        );
        assert_eq!(
            zones
                .resolve("Europe/Berlin", datetime!(2026-07-01 12:00:00))
                .unwrap(),
            ZoneResolution::Single(offset!(+2))
        );
    }

    #[test]
    fn offsets_are_read_back_on_both_sides_of_a_transition() {
        let zones = JiffZoneResolver::new();
        assert_eq!(
            zones
                .offset_at("Europe/Berlin", datetime!(2026-10-25 00:59:00 UTC))
                .unwrap(),
            offset!(+2)
        );
        assert_eq!(
            zones
                .offset_at("Europe/Berlin", datetime!(2026-10-25 01:01:00 UTC))
                .unwrap(),
            offset!(+1)
        );
    }

    #[test]
    fn a_zone_without_daylight_saving_is_one_offset_all_year() {
        let zones = JiffZoneResolver::new();
        for instant in [
            datetime!(2026-01-15 00:00:00 UTC),
            datetime!(2026-07-15 00:00:00 UTC),
        ] {
            assert_eq!(
                zones.offset_at("Asia/Shanghai", instant).unwrap(),
                offset!(+8)
            );
        }
    }

    #[test]
    fn an_unknown_zone_name_says_so_rather_than_defaulting_to_utc() {
        let zones = JiffZoneResolver::new();
        let error = zones
            .offset_at("Mars/Olympus_Mons", datetime!(2026-01-01 00:00:00 UTC))
            .unwrap_err();
        assert!(matches!(error, ZoneError::UnknownZone(_)), "{error}");
    }
}
