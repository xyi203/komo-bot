//! 时区：名字、解析结果，以及 croner 的日期时间适配器。
//!
//! kernel 不带 tzdb，所以这里只有**形状**：[`TimeZone`] 是一个 IANA 名字，
//! [`ZoneResolution`] 是"一个本地民用时间对应几个瞬时"的三种答案，[`FixedOffsetZone`]
//! 是没有 tzdb 时那个明确的降级。真正回答问题的是
//! [`ZoneResolver`](crate::traits::ZoneResolver)，由调用方传进来。

use std::cmp::Ordering;

use croner::time::{CivilDateTime, CronDateTime, Resolution, Weekday as CronWeekday};
use jiff::civil::DateTime;
use jiff::tz::Offset;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::traits::ZoneResolver;

/// 一个 IANA 时区名，例如 `Asia/Shanghai`。**只有名字**——偏移随日期变化，存下来就是
/// 一个会过期的快照。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(type = "string"))]
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
    Single(Offset),
    /// 对应两个：时钟往回拨，这段本地时间出现了两次。**先早后晚**（较早的那个瞬时
    /// 偏移更大）。§10：两次不同 UTC 时刻分别视为计划时间。
    Ambiguous(Offset, Offset),
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
    offset: Offset,
}

impl FixedOffsetZone {
    pub fn new(offset: Offset) -> Self {
        Self { offset }
    }

    pub fn utc() -> Self {
        Self {
            offset: Offset::UTC,
        }
    }
}

impl ZoneResolver for FixedOffsetZone {
    fn resolve(&self, _zone: &str, _local: DateTime) -> Result<ZoneResolution, ZoneError> {
        Ok(ZoneResolution::Single(self.offset))
    }

    fn offset_at(&self, _zone: &str, _instant: Timestamp) -> Result<Offset, ZoneError> {
        Ok(self.offset)
    }
}

/// 一段重复墙上时间：时钟往回拨的瞬时，以及它两侧的偏移。
#[derive(Debug, Clone, Copy)]
pub(super) struct LaterFold {
    /// 时钟往回拨的那个瞬时，也就是后一半的起点。
    pub(super) transition: Timestamp,
    /// 前一半（较早的孪生瞬时）用的偏移，较大的那个。
    pub(super) earlier_offset: Offset,
    /// 后一半用的偏移。
    pub(super) later_offset: Offset,
}

impl LaterFold {
    pub(super) fn shift(&self) -> SignedDuration {
        SignedDuration::from_secs(
            i64::from(self.earlier_offset.seconds()) - i64::from(self.later_offset.seconds()),
        )
    }

    /// 重复的那段墙上时间的起点。
    pub(super) fn wall_start(&self) -> DateTime {
        self.later_offset.to_datetime(self.transition)
    }
}

/// `after` 如果落在一段重复墙上时间的**前一半**里，返回那一段；否则 `None`。
///
/// 用 `offset_at` 在 24 小时窗口里二分找时钟往回拨的瞬时——没有哪个时区一次拨超过几个
/// 钟头，一天是很宽的余量，而这十几次 resolver 调用只在切换那天付。
pub(super) fn later_fold(
    after: Timestamp,
    zone: &str,
    zones: &dyn ZoneResolver,
) -> Result<Option<LaterFold>, ZoneError> {
    let offset = zones.offset_at(zone, after)?;
    let wall = offset.to_datetime(after);
    let ZoneResolution::Ambiguous(earlier_offset, later_offset) = zones.resolve(zone, wall)? else {
        return Ok(None);
    };
    // 已经在后一半上了，另一半在身后。
    if offset != earlier_offset || after >= assume_offset(wall, later_offset)? {
        return Ok(None);
    }

    let mut inside = after;
    let mut outside = after + SignedDuration::from_hours(24);
    if zones.offset_at(zone, outside)? == earlier_offset {
        // 一天之内没拨回来——不是这里该处理的情形。
        return Ok(None);
    }
    while (outside - inside).as_secs() > 1 {
        let middle = inside + SignedDuration::from_secs((outside - inside).as_secs() / 2);
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

/// 墙上时间 + 偏移 → 瞬时。偏移由 resolver 给出，结果越界只可能是 resolver 给了荒谬的偏移。
pub(super) fn assume_offset(local: DateTime, offset: Offset) -> Result<Timestamp, ZoneError> {
    offset
        .to_timestamp(local)
        .map_err(|error| ZoneError::Unavailable(error.to_string()))
}

/// `jiff::Timestamp` + 一个 [`ZoneResolver`] 的 [`CronDateTime`] 实现。
///
/// 内部按**瞬时**保存，`offset` 是这个瞬时在该区里的偏移（墙上时间由它算出），每次换瞬时都经
/// resolver 重取，所以跨过一次夏令时切换时它跟着变——这正是 croner 在绝对时间线上推进时需要的。
#[derive(Clone, Copy)]
pub(super) struct Zoned<'a> {
    instant: Timestamp,
    offset: Offset,
    zone: &'a str,
    zones: &'a dyn ZoneResolver,
}

impl<'a> Zoned<'a> {
    pub(super) fn at(
        instant: Timestamp,
        zone: &'a str,
        zones: &'a dyn ZoneResolver,
    ) -> Result<Self, ZoneError> {
        let offset = zones.offset_at(zone, instant)?;
        Ok(Zoned {
            instant,
            offset,
            zone,
            zones,
        })
    }

    pub(super) fn instant(&self) -> Timestamp {
        self.instant
    }

    fn with_instant(&self, instant: Timestamp) -> Self {
        let offset = self
            .zones
            .offset_at(self.zone, instant)
            .unwrap_or(self.offset);
        Zoned {
            instant,
            offset,
            zone: self.zone,
            zones: self.zones,
        }
    }

    fn resolved(&self, local: DateTime, offset: Offset) -> Result<Self, croner::errors::CronError> {
        Ok(Zoned {
            instant: offset
                .to_timestamp(local)
                .map_err(|_| croner::errors::CronError::InvalidDate)?,
            offset,
            zone: self.zone,
            zones: self.zones,
        })
    }
}

impl std::fmt::Debug for Zoned<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Zoned")
            .field("instant", &self.instant)
            .field("offset", &self.offset)
            .field("zone", &self.zone)
            .finish_non_exhaustive()
    }
}

impl CronDateTime for Zoned<'_> {
    fn to_civil(&self) -> CivilDateTime {
        let local = self.offset.to_datetime(self.instant);
        CivilDateTime::from_ymd_hms_opt(
            i32::from(local.year()),
            local.month() as u32,
            local.day() as u32,
            local.hour() as u32,
            local.minute() as u32,
            local.second() as u32,
        )
        .expect("civil::DateTime 的各部分本来就是合法日期")
    }

    fn civil_weekday(&self) -> CronWeekday {
        let local = self.offset.to_datetime(self.instant);
        CronWeekday::from_days_from_sunday(local.weekday().to_sunday_zero_offset() as u32)
    }

    fn resolve_civil(
        &self,
        civil: CivilDateTime,
    ) -> Result<Resolution<Self>, croner::errors::CronError> {
        let year =
            i16::try_from(civil.year()).map_err(|_| croner::errors::CronError::InvalidDate)?;
        let date = jiff::civil::Date::new(year, civil.month() as i8, civil.day() as i8)
            .map_err(|_| croner::errors::CronError::InvalidDate)?;
        let time = jiff::civil::Time::new(
            civil.hour() as i8,
            civil.minute() as i8,
            civil.second() as i8,
            0,
        )
        .map_err(|_| croner::errors::CronError::InvalidTime)?;
        let local = date.to_datetime(time);

        match self
            .zones
            .resolve(self.zone, local)
            .map_err(|_| croner::errors::CronError::InvalidDate)?
        {
            ZoneResolution::Single(offset) => Ok(Resolution::Single(self.resolved(local, offset)?)),
            ZoneResolution::Ambiguous(earlier, later) => {
                let mut first = self.resolved(local, earlier)?;
                let mut second = self.resolved(local, later)?;
                // croner 要求第一个是较早的瞬时；resolver 说错了也不至于让搜索乱掉。
                if first.instant > second.instant {
                    std::mem::swap(&mut first, &mut second);
                }
                Ok(Resolution::Ambiguous(first, second))
            }
            ZoneResolution::Gap => Ok(Resolution::Gap),
        }
    }

    fn checked_add_seconds(&self, seconds: i64) -> Option<Self> {
        self.instant
            .checked_add(SignedDuration::from_secs(seconds))
            .ok()
            .map(|instant| self.with_instant(instant))
    }

    fn cmp_instant(&self, other: &Self) -> Ordering {
        self.instant.cmp(&other.instant)
    }
}
