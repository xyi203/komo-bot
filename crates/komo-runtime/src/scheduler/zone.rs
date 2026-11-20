//! 真 tzdb 的 [`ZoneResolver`]（§10、§13.5）。
//!
//! kernel 不查 tzdb——它是一份随操作系统更新的数据，属于运行环境而不是领域——所以
//! 「Europe/Berlin 在 2026-03-29 02:30 是几点」这个问题由这里回答。jiff 自带一份
//! tzdb（系统有 `/usr/share/zoneinfo` 就用系统的），于是这个文件只做一件事：
//! **名字 → 规则**。
//!
//! 两个方向都要（§13.5）：`resolve` 是本地民用时间 → 瞬时（可能零个、一个或两个），
//! `offset_at` 是瞬时 → 本地偏移。§10 的两条夏令时规则由 kernel 的 `cron` 实现，这里
//! 只负责**把三种情形如实报上去**——把 `Fold` 报成 `Single` 就等于当这个区没有夏令时。

use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::Offset;
use komo_kernel::cron::{ZoneError, ZoneResolution};
use komo_kernel::traits::ZoneResolver;

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
    fn resolve(&self, zone: &str, local: DateTime) -> Result<ZoneResolution, ZoneError> {
        use jiff::tz::AmbiguousOffset;

        let tz = Self::zone(zone)?;
        Ok(match tz.to_ambiguous_timestamp(local).offset() {
            AmbiguousOffset::Unambiguous { offset } => ZoneResolution::Single(offset),
            // 时钟往回拨：`before` 是较早那个瞬时的偏移（更大的那个），`after` 是较晚
            // 的——正是 kernel 要求的"先早后晚"。
            AmbiguousOffset::Fold { before, after } => ZoneResolution::Ambiguous(before, after),
            // 时钟往前拨：这个本地时间不存在。§10 说跳过，所以这里如实报 Gap，不替
            // 调用方挑一个"最接近的"瞬时。
            AmbiguousOffset::Gap { .. } => ZoneResolution::Gap,
        })
    }

    fn offset_at(&self, zone: &str, instant: Timestamp) -> Result<Offset, ZoneError> {
        Ok(Self::zone(zone)?.to_offset(instant))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::cron::{ScheduleError, TimeZone, Trigger, next_occurrence, parse_schedule};

    /// 这五条和 kernel `test_support::zones` 里那五条是**同一组断言**，只是那边跑的是
    /// 可编脚本的假时区，这里跑的是真 tzdb 的 Europe/Berlin。假的答对了而真的答错，
    /// 说明这个 resolver 有问题；反过来说明 kernel 的规则实现有问题。
    fn berlin() -> TimeZone {
        TimeZone::new("Europe/Berlin")
    }

    /// 这个瞬时在柏林的墙上时间与偏移。
    fn wall(at: Timestamp) -> (DateTime, Offset) {
        let offset = JiffZoneResolver::new()
            .offset_at("Europe/Berlin", at)
            .unwrap();
        (offset.to_datetime(at), offset)
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

        let before = "2026-10-24T12:00:00Z".parse::<jiff::Timestamp>().unwrap();
        let first = next_occurrence(&trigger, before, &zones).unwrap().unwrap();
        let second = next_occurrence(&trigger, first, &zones).unwrap().unwrap();

        assert_eq!(
            first,
            "2026-10-25T00:30:00Z".parse::<jiff::Timestamp>().unwrap()
        );
        assert_eq!(
            second,
            "2026-10-25T01:30:00Z".parse::<jiff::Timestamp>().unwrap()
        );
        assert_ne!(first, second, "两个不同的 UTC 时刻");
        let (first_wall, first_offset) = wall(first);
        let (second_wall, second_offset) = wall(second);
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
        let zones = JiffZoneResolver::new();
        let next = next_occurrence(
            &daily_at_0230(),
            "2026-03-28T12:00:00Z".parse::<jiff::Timestamp>().unwrap(),
            &zones,
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            next,
            "2026-03-30T00:30:00Z".parse::<jiff::Timestamp>().unwrap()
        );
        let (next_wall, _) = wall(next);
        assert_eq!(next_wall.day(), 30, "29 日那一槽被跳过，不是挪到 03:30");
        assert_eq!(next_wall.hour(), 2);
        assert_eq!(next_wall.minute(), 30);
    }

    /// 一次性触发写到一个不存在的本地时间上——当场拒绝，趁打字的人还在。
    #[test]
    fn a_one_shot_at_a_nonexistent_local_time_is_refused() {
        let zones = JiffZoneResolver::new();
        let err = parse_schedule(
            "@at 2026-03-29 02:30",
            &berlin(),
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
        let zones = JiffZoneResolver::new();
        let trigger = parse_schedule(
            "@at 2026-10-25 02:30",
            &berlin(),
            "2026-10-01T00:00:00Z".parse::<jiff::Timestamp>().unwrap(),
            &zones,
        )
        .unwrap();
        let Trigger::At { at } = trigger else {
            panic!("@at 解析成了别的东西")
        };
        assert_eq!(
            at,
            "2026-10-25T00:30:00Z".parse::<jiff::Timestamp>().unwrap()
        );
    }

    /// 切换日之外的日子照常，一天一次。
    #[test]
    fn an_ordinary_day_still_fires_once() {
        let zones = JiffZoneResolver::new();
        let first = next_occurrence(
            &daily_at_0230(),
            "2026-07-01T12:00:00Z".parse::<jiff::Timestamp>().unwrap(),
            &zones,
        )
        .unwrap()
        .unwrap();
        let second = next_occurrence(&daily_at_0230(), first, &zones)
            .unwrap()
            .unwrap();
        assert_eq!(second - first, jiff::SignedDuration::from_hours(24));
        assert_eq!(wall(first).1, jiff::tz::offset(2), "七月是夏令时");
    }

    // ---- resolver 自己的两个方向 ----

    #[test]
    fn the_three_resolutions_are_reported_as_they_are() {
        let zones = JiffZoneResolver::new();
        assert_eq!(
            zones
                .resolve(
                    "Europe/Berlin",
                    jiff::civil::date(2026, 3, 29).at(2, 30, 0, 0)
                )
                .unwrap(),
            ZoneResolution::Gap
        );
        assert_eq!(
            zones
                .resolve(
                    "Europe/Berlin",
                    jiff::civil::date(2026, 10, 25).at(2, 30, 0, 0)
                )
                .unwrap(),
            ZoneResolution::Ambiguous(jiff::tz::offset(2), jiff::tz::offset(1)),
            "先早后晚"
        );
        assert_eq!(
            zones
                .resolve(
                    "Europe/Berlin",
                    jiff::civil::date(2026, 7, 1).at(12, 0, 0, 0)
                )
                .unwrap(),
            ZoneResolution::Single(jiff::tz::offset(2))
        );
    }

    #[test]
    fn offsets_are_read_back_on_both_sides_of_a_transition() {
        let zones = JiffZoneResolver::new();
        assert_eq!(
            zones
                .offset_at(
                    "Europe/Berlin",
                    "2026-10-25T00:59:00Z".parse::<jiff::Timestamp>().unwrap()
                )
                .unwrap(),
            jiff::tz::offset(2)
        );
        assert_eq!(
            zones
                .offset_at(
                    "Europe/Berlin",
                    "2026-10-25T01:01:00Z".parse::<jiff::Timestamp>().unwrap()
                )
                .unwrap(),
            jiff::tz::offset(1)
        );
    }

    #[test]
    fn a_zone_without_daylight_saving_is_one_offset_all_year() {
        let zones = JiffZoneResolver::new();
        for instant in [
            "2026-01-15T00:00:00Z".parse::<jiff::Timestamp>().unwrap(),
            "2026-07-15T00:00:00Z".parse::<jiff::Timestamp>().unwrap(),
        ] {
            assert_eq!(
                zones.offset_at("Asia/Shanghai", instant).unwrap(),
                jiff::tz::offset(8)
            );
        }
    }

    #[test]
    fn an_unknown_zone_name_says_so_rather_than_defaulting_to_utc() {
        let zones = JiffZoneResolver::new();
        let error = zones
            .offset_at(
                "Mars/Olympus_Mons",
                "2026-01-01T00:00:00Z".parse::<jiff::Timestamp>().unwrap(),
            )
            .unwrap_err();
        assert!(matches!(error, ZoneError::UnknownZone(_)), "{error}");
    }
}
