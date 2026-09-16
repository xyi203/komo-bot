//! 时区：名字、解析结果，以及 croner 的日期时间适配器。
//!
//! kernel 不带 tzdb，所以这里只有**形状**：[`TimeZone`] 是一个 IANA 名字，
//! [`ZoneResolution`] 是"一个本地民用时间对应几个瞬时"的三种答案，[`FixedOffsetZone`]
//! 是没有 tzdb 时那个明确的降级。真正回答问题的是
//! [`ZoneResolver`](crate::traits::ZoneResolver)，由调用方传进来。

use std::cmp::Ordering;

use croner::time::{CivilDateTime, CronDateTime, Resolution, Weekday as CronWeekday};
use serde::{Deserialize, Serialize};
use time::{Date, Duration, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

use crate::traits::ZoneResolver;

/// 一个 IANA 时区名，例如 `Asia/Shanghai`。**只有名字**——偏移随日期变化，存下来就是
/// 一个会过期的快照。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TimeZone(String);

impl TimeZone {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn utc() -> Self {
        Self("UTC".into())
    }

    pub fn name(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TimeZone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 一个本地民用时间在某个区里对应几个瞬时。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneResolution {
    /// 只对应一个瞬时——绝大多数时候。
    Single(UtcOffset),
    /// 对应两个：时钟往回拨，这段本地时间出现了两次。**先早后晚**（较早的那个瞬时
    /// 偏移更大）。§10：两次不同 UTC 时刻分别视为计划时间。
    Ambiguous(UtcOffset, UtcOffset),
    /// 一个都不对应：时钟往前拨，这个本地时间不存在。§10：跳过。
    Gap,
}

/// 时区解析失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ZoneError {
    /// 这个实现不认识这个 IANA 名字。
    #[error("未知时区：{0}")]
    UnknownZone(String),
    #[error("时区数据不可用：{0}")]
    Unavailable(String),
}

/// 一个**固定偏移**的时区实现：名字随便叫，永远只有一个偏移。
///
/// 两个用处：`UTC` 本身（它确实固定），以及在没有 tzdb 的环境里当明确的回退。它**答
/// 不出** [`ZoneResolution::Ambiguous`] / [`ZoneResolution::Gap`]，所以拿它跑一个有夏令
/// 时的区，得到的就是"当这个区没有夏令时"——这是一个应当被知道的降级，不是默认行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedOffsetZone {
    offset: UtcOffset,
}

impl FixedOffsetZone {
    pub fn new(offset: UtcOffset) -> Self {
        Self { offset }
    }

    pub fn utc() -> Self {
        Self {
            offset: UtcOffset::UTC,
        }
    }
}

impl ZoneResolver for FixedOffsetZone {
    fn resolve(&self, _zone: &str, _local: PrimitiveDateTime) -> Result<ZoneResolution, ZoneError> {
        Ok(ZoneResolution::Single(self.offset))
    }

    fn offset_at(&self, _zone: &str, _instant: OffsetDateTime) -> Result<UtcOffset, ZoneError> {
        Ok(self.offset)
    }
}

/// 一段重复墙上时间：时钟往回拨的瞬时，以及它两侧的偏移。
#[derive(Debug, Clone, Copy)]
pub(super) struct LaterFold {
    /// 时钟往回拨的那个瞬时，也就是后一半的起点。
    pub(super) transition: OffsetDateTime,
    /// 前一半（较早的孪生瞬时）用的偏移，较大的那个。
    pub(super) earlier_offset: UtcOffset,
    /// 后一半用的偏移。
    pub(super) later_offset: UtcOffset,
}

impl LaterFold {
    pub(super) fn shift(&self) -> Duration {
        Duration::seconds(
            (self.earlier_offset.whole_seconds() - self.later_offset.whole_seconds()) as i64,
        )
    }

    /// 重复的那段墙上时间的起点。
    pub(super) fn wall_start(&self) -> PrimitiveDateTime {
        let local = self.transition.to_offset(self.later_offset);
        PrimitiveDateTime::new(local.date(), local.time())
    }
}

/// `after` 如果落在一段重复墙上时间的**前一半**里，返回那一段；否则 `None`。
///
/// 用 `offset_at` 在 24 小时窗口里二分找时钟往回拨的瞬时——没有哪个时区一次拨超过几个
/// 钟头，一天是很宽的余量，而这十几次 resolver 调用只在切换那天付。
pub(super) fn later_fold(
    after: OffsetDateTime,
    zone: &str,
    zones: &dyn ZoneResolver,
) -> Result<Option<LaterFold>, ZoneError> {
    let offset = zones.offset_at(zone, after)?;
    let local = after.to_offset(offset);
    let wall = PrimitiveDateTime::new(local.date(), local.time());
    let ZoneResolution::Ambiguous(earlier_offset, later_offset) = zones.resolve(zone, wall)? else {
        return Ok(None);
    };
    // 已经在后一半上了，另一半在身后。
    if offset != earlier_offset || after >= wall.assume_offset(later_offset) {
        return Ok(None);
    }

    let mut inside = after;
    let mut outside = after + Duration::hours(24);
    if zones.offset_at(zone, outside)? == earlier_offset {
        // 一天之内没拨回来——不是这里该处理的情形。
        return Ok(None);
    }
    while (outside - inside).whole_seconds() > 1 {
        let middle = inside + Duration::seconds((outside - inside).whole_seconds() / 2);
        if zones.offset_at(zone, middle)? == earlier_offset {
            inside = middle;
        } else {
            outside = middle;
        }
    }
    Ok(Some(LaterFold {
        transition: outside,
        earlier_offset,
        later_offset,
    }))
}

/// `time::OffsetDateTime` + 一个 [`ZoneResolver`] 的 [`CronDateTime`] 实现。
///
/// 内部按**瞬时**保存（`instant` 的偏移只是渲染用），墙上时间每次经 resolver 算出来，
/// 所以跨过一次夏令时切换时它跟着变——这正是 croner 在绝对时间线上推进时需要的。
#[derive(Clone, Copy)]
pub(super) struct Zoned<'a> {
    instant: OffsetDateTime,
    zone: &'a str,
    zones: &'a dyn ZoneResolver,
}

impl<'a> Zoned<'a> {
    pub(super) fn at(
        instant: OffsetDateTime,
        zone: &'a str,
        zones: &'a dyn ZoneResolver,
    ) -> Result<Self, ZoneError> {
        let offset = zones.offset_at(zone, instant)?;
        Ok(Zoned {
            instant: instant.to_offset(offset),
            zone,
            zones,
        })
    }

    /// 带着它自己那个区的偏移的时刻——调用方看到的就是"在它自己的时区里几点"。
    pub(super) fn local(&self) -> OffsetDateTime {
        self.instant
    }

    fn with_instant(&self, instant: OffsetDateTime) -> Self {
        let offset = self
            .zones
            .offset_at(self.zone, instant)
            .unwrap_or_else(|_| instant.offset());
        Zoned {
            instant: instant.to_offset(offset),
            zone: self.zone,
            zones: self.zones,
        }
    }
}

impl std::fmt::Debug for Zoned<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Zoned")
            .field("instant", &self.instant)
            .field("zone", &self.zone)
            .finish_non_exhaustive()
    }
}

impl CronDateTime for Zoned<'_> {
    fn to_civil(&self) -> CivilDateTime {
        CivilDateTime::from_ymd_hms_opt(
            self.instant.year(),
            self.instant.month() as u32,
            self.instant.day() as u32,
            self.instant.hour() as u32,
            self.instant.minute() as u32,
            self.instant.second() as u32,
        )
        .expect("OffsetDateTime 的各部分本来就是合法日期")
    }

    fn civil_weekday(&self) -> CronWeekday {
        CronWeekday::from_days_from_sunday(self.instant.weekday().number_days_from_sunday() as u32)
    }

    fn resolve_civil(
        &self,
        civil: CivilDateTime,
    ) -> Result<Resolution<Self>, croner::errors::CronError> {
        let month = Month::try_from(civil.month() as u8)
            .map_err(|_| croner::errors::CronError::InvalidDate)?;
        let date = Date::from_calendar_date(civil.year(), month, civil.day() as u8)
            .map_err(|_| croner::errors::CronError::InvalidDate)?;
        let time = Time::from_hms(
            civil.hour() as u8,
            civil.minute() as u8,
            civil.second() as u8,
        )
        .map_err(|_| croner::errors::CronError::InvalidTime)?;
        let local = PrimitiveDateTime::new(date, time);

        match self
            .zones
            .resolve(self.zone, local)
            .map_err(|_| croner::errors::CronError::InvalidDate)?
        {
            ZoneResolution::Single(offset) => Ok(Resolution::Single(Zoned {
                instant: local.assume_offset(offset),
                zone: self.zone,
                zones: self.zones,
            })),
            ZoneResolution::Ambiguous(earlier, later) => {
                let mut first = local.assume_offset(earlier);
                let mut second = local.assume_offset(later);
                // croner 要求第一个是较早的瞬时；resolver 说错了也不至于让搜索乱掉。
                if first > second {
                    std::mem::swap(&mut first, &mut second);
                }
                Ok(Resolution::Ambiguous(
                    Zoned {
                        instant: first,
                        zone: self.zone,
                        zones: self.zones,
                    },
                    Zoned {
                        instant: second,
                        zone: self.zone,
                        zones: self.zones,
                    },
                ))
            }
            ZoneResolution::Gap => Ok(Resolution::Gap),
        }
    }

    fn checked_add_seconds(&self, seconds: i64) -> Option<Self> {
        self.instant
            .checked_add(Duration::seconds(seconds))
            .map(|instant| self.with_instant(instant))
    }

    fn cmp_instant(&self, other: &Self) -> Ordering {
        self.instant
            .unix_timestamp_nanos()
            .cmp(&other.instant.unix_timestamp_nanos())
    }
}
