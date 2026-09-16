//! Cron：触发器、到期计算与调度表达式解析（§10）。
//!
//! **kernel 不带时区数据库。**`time` 只能表示一个固定偏移，IANA 名字到偏移的解析需要
//! tzdb，那是上层的事。所以 [`TimeZone`] 只存**名字**（写进 `cron_jobs`，是权威），而
//! 「这个本地时间在这个区里对应哪个瞬时」由调用方传进来的
//! [`ZoneResolver`](crate::traits::ZoneResolver) 回答。
//!
//! 这个 port 的形状就是 §10 那两条夏令时规则要求的形状：一个本地民用时间在一个区里
//! 可能对应**一个**瞬时、**两个**瞬时（时钟往回拨，同一日历时间出现两次，两次分别
//! 视为计划时间），或者**一个都没有**（时钟往前拨，这个本地时间不存在，跳过）。
//! [`ZoneResolution`] 把这三种分开，croner 4 的 `CronDateTime::resolve_civil` 正好也
//! 分这三种，中间不丢信息。
//!
//! croner 用 `default-features = false`：默认特性会拉进 chrono，而 kernel 只允许一套
//! 日期时间库（§13.4）。去掉 chrono 之后 croner 依然可用——它 4.0 起把搜索建在自己
//! 的民用时间类型上，日期库通过 [`croner::time::CronDateTime`] 接进去，下面的
//! [`Zoned`] 就是 `time::OffsetDateTime` + 一个 `ZoneResolver` 的那个实现。

mod zone;

use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use serde::{Deserialize, Serialize};
use time::{Date, Duration, Month, OffsetDateTime, PrimitiveDateTime, Time};

pub use zone::{FixedOffsetZone, TimeZone, ZoneError, ZoneResolution};
use zone::{LaterFold, Zoned, later_fold};

use crate::traits::ZoneResolver;
use crate::types::ids::{CronJobId, RunId, SessionId};
use crate::types::model::{Effort, ModelConfig};

/// 什么让一个 Job 触发。
///
/// 它只会给出一个**时刻**或者不给：`next_run_at` 永远握着一个调度器找得到的槽位。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    /// 五字段 cron 表达式，按 `tz` 的本地时间匹配。
    Cron { expr: String, tz: TimeZone },
    /// 一次性：`@at YYYY-MM-DD HH:MM`，创建时就解析成了它的瞬时。
    At {
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
}

/// 解析调度表达式或计算到期失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("调度表达式为空")]
    Empty,
    #[error("cron 表达式必须是五个字段（分 时 日 月 周），收到 {found} 个：{input}")]
    NotFiveFields { input: String, found: usize },
    #[error("cron 表达式不合法：{message}")]
    InvalidCron { message: String },
    #[error("`@at` 的时间要写成 `@at YYYY-MM-DD HH:MM`，收到：{input}")]
    InvalidAt { input: String },
    #[error("`@at {at}` 已经过去了")]
    AtInThePast { at: String },
    /// 时钟往前拨，这个本地时间在这个区里不存在。一次性触发当场拒绝——循环触发是跳过。
    #[error("{at} 在 {zone} 不存在（时钟在这里往前拨了）")]
    LocalTimeDoesNotExist { at: String, zone: String },
    #[error("只接受五字段 cron 表达式或 `@at YYYY-MM-DD HH:MM`，收到：{input}")]
    Unsupported { input: String },
    #[error(transparent)]
    Zone(#[from] ZoneError),
    /// croner 在给定范围内找不到匹配（例如 2 月 30 日）。
    #[error("找不到匹配的时刻：{0}")]
    NoOccurrence(String),
}

/// 把一个调度表达式字符串解析成 [`Trigger`]。
///
/// **只接受两种写法**（§10）：
///
/// - 五字段 cron 表达式，例如 `0 9 * * *`；秒和年都不接受——多一个字段就是另一种
///   语义，静默按六字段解释会让"每天 9 点"变成"每分钟的第 9 秒"。
/// - `@at YYYY-MM-DD HH:MM`，按 `zone` 解释，解析成它的瞬时。
///
/// `now` 是判断"这个 `@at` 是不是已经过去了"的基准——过去的一次性时间当场拒绝，趁
/// 打字的人还在。写了一个**不存在**的本地时间也当场拒绝；落在时钟往回拨的重复区间
/// 里则取较早的那个瞬时（两个都是真的，早的那个更接近人所说的"到时候"）。
pub fn parse_schedule(
    input: &str,
    zone: &TimeZone,
    now: OffsetDateTime,
    zones: &dyn ZoneResolver,
) -> Result<Trigger, ScheduleError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ScheduleError::Empty);
    }

    if let Some(rest) = trimmed.strip_prefix("@at") {
        let at = parse_at(rest.trim(), zone, zones)?;
        if at <= now {
            return Err(ScheduleError::AtInThePast {
                at: rest.trim().to_string(),
            });
        }
        return Ok(Trigger::At { at });
    }

    if trimmed.starts_with('@') {
        // `@daily` 一类的别名不在两种写法里。
        return Err(ScheduleError::Unsupported {
            input: trimmed.to_string(),
        });
    }

    validate_cron(trimmed)?;
    // 表达式合法还不够：区名也得是这个 resolver 认识的，否则这个 Job 到点不会响，而
    // 发现它的时候人已经走了。
    zones.offset_at(zone.name(), now)?;
    Ok(Trigger::Cron {
        expr: trimmed.to_string(),
        tz: zone.clone(),
    })
}

/// 五字段语义验证：字段数对，且 croner 能解析。
pub fn validate_cron(expr: &str) -> Result<Cron, ScheduleError> {
    let fields = expr.split_whitespace().count();
    if fields != 5 {
        return Err(ScheduleError::NotFiveFields {
            input: expr.to_string(),
            found: fields,
        });
    }
    five_field_parser()
        .parse(expr)
        .map_err(|e| ScheduleError::InvalidCron {
            message: e.to_string(),
        })
}

fn five_field_parser() -> CronParser {
    CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
}

fn parse_at(
    raw: &str,
    zone: &TimeZone,
    zones: &dyn ZoneResolver,
) -> Result<OffsetDateTime, ScheduleError> {
    let invalid = || ScheduleError::InvalidAt {
        input: raw.to_string(),
    };
    let (date_part, time_part) = raw.split_once(' ').ok_or_else(invalid)?;
    let mut date = date_part.split('-');
    let (year, month, day) = (
        date.next().ok_or_else(invalid)?,
        date.next().ok_or_else(invalid)?,
        date.next().ok_or_else(invalid)?,
    );
    if date.next().is_some() {
        return Err(invalid());
    }
    let (hour, minute) = time_part.split_once(':').ok_or_else(invalid)?;
    if minute.contains(':') {
        return Err(invalid());
    }

    let year: i32 = year.parse().map_err(|_| invalid())?;
    let month: u8 = month.parse().map_err(|_| invalid())?;
    let day: u8 = day.parse().map_err(|_| invalid())?;
    let hour: u8 = hour.parse().map_err(|_| invalid())?;
    let minute: u8 = minute.parse().map_err(|_| invalid())?;

    let month = Month::try_from(month).map_err(|_| invalid())?;
    let date = Date::from_calendar_date(year, month, day).map_err(|_| invalid())?;
    let time = Time::from_hms(hour, minute, 0).map_err(|_| invalid())?;
    let local = PrimitiveDateTime::new(date, time);

    match zones.resolve(zone.name(), local)? {
        ZoneResolution::Single(offset) => Ok(local.assume_offset(offset)),
        // 两个瞬时都是真的；取较早的那个。
        ZoneResolution::Ambiguous(earlier, _) => Ok(local.assume_offset(earlier)),
        ZoneResolution::Gap => Err(ScheduleError::LocalTimeDoesNotExist {
            at: raw.to_string(),
            zone: zone.name().to_string(),
        }),
    }
}

/// `after` **之后**（不含）的下一个到期时刻，用触发器自己的时区表达。
///
/// 一次性触发器在它自己的时刻之后就没有下一次了，返回 `Ok(None)`——这正是 §10 里
/// 「`@at` 是一次性，claim 时完成」所要的：没有下一槽就不会再被找到。
///
/// **§10 的两条夏令时规则在这里落地，它们都不是 croner 的默认策略**，所以是这里的两
/// 段代码而不是一个配置项：
///
/// - **不存在的本地时间跳过。**croner 对固定时刻的任务会把缺口里的那一槽抬到缺口之后
///   的第一个真实瞬时（它自己的测试就叫 `fixed_time_gap_search_uses_the_post_gap_instant`），
///   于是"每天 02:30"在春季切换那天变成 03:00 跑。§10 要的是那天不跑。被抬起来的结果
///   有一个准确的指纹：它的**墙上时间不再匹配表达式**，所以认出它、从它往后再找一次
///   就是了。
/// - **重复的本地时间两次分别算。**croner 对固定时刻的前向搜索会直接短路掉"走另一半"
///   那一步（源码注释：固定时刻的任务在后一半不跑）。§10 要的是两个 UTC 时刻各算一次，
///   所以时钟往回拨那天额外从**后一半的起点**再走一遍，取两个答案里近的那个。
pub fn next_occurrence(
    trigger: &Trigger,
    after: OffsetDateTime,
    zones: &dyn ZoneResolver,
) -> Result<Option<OffsetDateTime>, ScheduleError> {
    match trigger {
        Trigger::At { at } => Ok((*at > after).then_some(*at)),
        Trigger::Cron { expr, tz } => {
            let cron = validate_cron(expr)?;
            let mut best = search_from(&cron, after, false, tz.name(), zones)?;

            // 时钟往回拨的那天，后一半的那个瞬时前向走不到——croner 对固定时刻的任务
            // 只认较早的那个孪生瞬时（源码：「A fixed-time job runs once, at the earlier
            // of the two instants」）。§10 要两次都算，所以这里自己把后一半那次算出来。
            if let Some(fold) = later_fold(after, tz.name(), zones)?
                && let Some(twin) = repeated_twin(&cron, &fold, after, tz.name(), zones)?
            {
                best = Some(match best {
                    Some(found) if found <= twin => found,
                    _ => twin,
                });
            }
            Ok(best)
        }
    }
}

/// 重复那段墙上时间里，第一个匹配表达式的墙上时刻的**后一半**那个瞬时。
///
/// 在**前一半**里搜（croner 对固定时刻的任务返回的正是较早的孪生瞬时），拿到墙上时间，
/// 再按后一半的偏移换算一次。重复区间里任何一个匹配的墙上时刻，它的后一半瞬时都晚于
/// `after`——`after` 自己就在前一半里——所以第一个就是最近的那个。
fn repeated_twin(
    cron: &Cron,
    fold: &LaterFold,
    after: OffsetDateTime,
    zone: &str,
    zones: &dyn ZoneResolver,
) -> Result<Option<OffsetDateTime>, ScheduleError> {
    let wall_start = fold.wall_start();
    let first_earlier = fold.transition - fold.shift();
    let Some(found) = search_from(
        cron,
        first_earlier - Duration::seconds(1),
        false,
        zone,
        zones,
    )?
    else {
        return Ok(None);
    };

    let local = found.to_offset(fold.earlier_offset);
    let wall = PrimitiveDateTime::new(local.date(), local.time());
    if wall < wall_start || wall >= wall_start + fold.shift() {
        // 这一段里没有匹配的墙上时刻。
        return Ok(None);
    }
    let twin = wall.assume_offset(fold.later_offset);
    Ok((twin > after).then_some(twin))
}

/// 一次搜索，外加"跳过不存在的本地时间"。
fn search_from(
    cron: &Cron,
    from: OffsetDateTime,
    inclusive: bool,
    zone: &str,
    zones: &dyn ZoneResolver,
) -> Result<Option<OffsetDateTime>, ScheduleError> {
    /// 连着八次都落在缺口里是不可能的；到这个数说明别处错了，报错胜过默默返回 None。
    const MAX_GAP_SKIPS: u32 = 8;

    let mut cursor = from;
    let mut inclusive = inclusive;
    for _ in 0..MAX_GAP_SKIPS {
        let start = Zoned::at(cursor, zone, zones)?;
        let found = match cron.find_next_occurrence(&start, inclusive) {
            Ok(found) => found,
            Err(croner::errors::CronError::TimeSearchLimitExceeded) => return Ok(None),
            Err(e) => return Err(ScheduleError::NoOccurrence(e.to_string())),
        };
        // 被缺口抬起来的结果，墙上时间不再匹配表达式。
        if cron.is_time_matching(&found).unwrap_or(true) {
            return Ok(Some(found.local()));
        }
        cursor = found.local();
        inclusive = false;
    }
    Err(ScheduleError::NoOccurrence(format!(
        "连续 {MAX_GAP_SKIPS} 次落在不存在的本地时间上，{zone} 的时区数据可能有问题"
    )))
}

/// 这个时刻是不是触发器的一个计划时间。
///
/// 被缺口抬起来的那种瞬时在这里答 `false`——它的墙上时间不匹配表达式，而 §10 说那一槽
/// 是跳过的。
pub fn matches(
    trigger: &Trigger,
    at: OffsetDateTime,
    zones: &dyn ZoneResolver,
) -> Result<bool, ScheduleError> {
    match trigger {
        Trigger::At { at: scheduled } => Ok(scheduled == &at),
        Trigger::Cron { expr, tz } => {
            let cron = validate_cron(expr)?;
            let moment = Zoned::at(at, tz.name(), zones)?;
            cron.is_time_matching(&moment)
                .map_err(|e| ScheduleError::NoOccurrence(e.to_string()))
        }
    }
}

/// 同一 Job 上一次还没结束时怎么办（§10）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlapPolicy {
    /// 跳过本次并记录原因。默认——「含等待审批、重试或结果核对」都算没结束。
    #[default]
    Skip,
    /// 照常再起一个。
    Allow,
}

/// Job 的生命周期。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Active,
    Paused,
    /// 一次性触发已经完成，行留着当可查询的记录。
    Done,
}

/// 一个定时任务（`cron_jobs`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CronJob {
    pub id: CronJobId,
    pub name: String,
    /// 定义版本。Job 改了，绑定它的授权失效（§7.2）。
    pub version: u64,
    pub trigger: Trigger,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<std::path::PathBuf>,
    pub status: JobStatus,
    #[serde(default)]
    pub overlap: OverlapPolicy,
    /// 本 Job 的主模型覆盖。**按完整模型配置解析**，不影响记忆整理或向量模型（§10）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// 触发时预载进首轮上下文的 SKILL.md（§5.6）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// 执行预算：最大轮数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
    /// 下一个槽位。`next_occurrence` 算出来的那个。
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub next_run_at: Option<OffsetDateTime>,
    /// 触发 / 配置层面的问题。执行失败记在 firing 上，不记这里。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl CronJob {
    /// 在 `now` 这一刻该不该触发。
    pub fn is_due(&self, now: OffsetDateTime) -> bool {
        self.status == JobStatus::Active && self.next_run_at.is_some_and(|slot| slot <= now)
    }

    /// 重算下一个槽位。算不出来时把 `next_run_at` 清空并把原因写进 `last_error`——一个
    /// 区名解析不了的 Job 应当在清单里看得见，而不是安静地再也不响。
    pub fn advance(
        &mut self,
        after: OffsetDateTime,
        zones: &dyn ZoneResolver,
    ) -> Result<(), ScheduleError> {
        match next_occurrence(&self.trigger, after, zones) {
            Ok(slot) => {
                self.next_run_at = slot;
                self.last_error = None;
                Ok(())
            }
            Err(error) => {
                self.next_run_at = None;
                self.last_error = Some(error.to_string());
                Err(error)
            }
        }
    }
}

/// 一次触发（`cron_firings`）。唯一键是 `job_id + scheduled_at_utc`——同一计划时间不
/// 重复创建运行（§10）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CronFiring {
    pub job: CronJobId,
    pub job_version: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub scheduled_at: OffsetDateTime,
    /// 不可变的触发快照：据此可以补完尚未写入的触发输入（§8.5）。
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::UtcOffset;
    use time::macros::{datetime, offset};

    fn shanghai() -> TimeZone {
        TimeZone::new("Asia/Shanghai")
    }

    /// 上海没有夏令时，固定 +8 就是它的全部真相。
    fn shanghai_zone() -> FixedOffsetZone {
        FixedOffsetZone::new(offset!(+8))
    }

    const NOW: OffsetDateTime = datetime!(2026-09-15 08:00:00 UTC);

    #[test]
    fn a_five_field_expression_parses() {
        let trigger = parse_schedule("0 9 * * *", &shanghai(), NOW, &shanghai_zone()).unwrap();
        assert_eq!(
            trigger,
            Trigger::Cron {
                expr: "0 9 * * *".into(),
                tz: shanghai()
            }
        );
    }

    #[test]
    fn six_fields_are_refused_rather_than_read_as_seconds() {
        let err = parse_schedule("0 0 9 * * *", &shanghai(), NOW, &shanghai_zone()).unwrap_err();
        assert!(
            matches!(err, ScheduleError::NotFiveFields { found: 6, .. }),
            "{err}"
        );
    }

    #[test]
    fn nicknames_are_not_one_of_the_two_accepted_spellings() {
        assert!(matches!(
            parse_schedule("@daily", &shanghai(), NOW, &shanghai_zone()),
            Err(ScheduleError::Unsupported { .. })
        ));
    }

    #[test]
    fn a_nonsense_expression_is_rejected() {
        assert!(matches!(
            parse_schedule("0 99 * * *", &shanghai(), NOW, &shanghai_zone()),
            Err(ScheduleError::InvalidCron { .. })
        ));
    }

    #[test]
    fn an_at_trigger_resolves_to_its_instant_in_the_jobs_zone() {
        let trigger =
            parse_schedule("@at 2026-09-16 09:00", &shanghai(), NOW, &shanghai_zone()).unwrap();
        let Trigger::At { at } = trigger else {
            panic!()
        };
        // 上海的 9 点是 UTC 的 1 点。
        assert_eq!(
            at.to_offset(UtcOffset::UTC),
            datetime!(2026-09-16 01:00:00 UTC)
        );
    }

    #[test]
    fn an_at_in_the_past_is_refused_while_the_person_is_still_there() {
        let err =
            parse_schedule("@at 2026-09-14 09:00", &shanghai(), NOW, &shanghai_zone()).unwrap_err();
        assert!(matches!(err, ScheduleError::AtInThePast { .. }), "{err}");
    }

    #[test]
    fn a_malformed_at_says_what_the_spelling_is() {
        for input in [
            "@at 2026-09-16",
            "@at 2026/09/16 09:00",
            "@at 2026-09-16 09:00:00",
            "@at 2026-02-30 09:00",
            "@at 2026-09-16 25:00",
        ] {
            assert!(
                matches!(
                    parse_schedule(input, &shanghai(), NOW, &shanghai_zone()),
                    Err(ScheduleError::InvalidAt { .. })
                ),
                "{input}"
            );
        }
    }

    #[test]
    fn the_next_occurrence_of_a_daily_job_is_in_its_own_zone() {
        let trigger = parse_schedule("0 9 * * *", &shanghai(), NOW, &shanghai_zone()).unwrap();
        // NOW 是 UTC 08:00 = 上海 16:00，所以下一次是明天上海 09:00。
        let next = next_occurrence(&trigger, NOW, &shanghai_zone())
            .unwrap()
            .unwrap();
        assert_eq!(
            next.to_offset(UtcOffset::UTC),
            datetime!(2026-09-16 01:00:00 UTC)
        );
        assert_eq!(next.offset(), offset!(+8));
        assert_eq!(next.hour(), 9, "在它自己的时区里就是 9 点");
    }

    #[test]
    fn the_next_occurrence_is_strictly_after_the_given_moment() {
        let utc = FixedOffsetZone::utc();
        let trigger = parse_schedule("0 9 * * *", &TimeZone::utc(), NOW, &utc).unwrap();
        let nine = datetime!(2026-09-15 09:00:00 UTC);
        assert_eq!(
            next_occurrence(&trigger, nine, &utc).unwrap().unwrap(),
            datetime!(2026-09-16 09:00:00 UTC)
        );
        assert!(matches(&trigger, nine, &utc).unwrap());
    }

    #[test]
    fn a_weekday_expression_lands_on_the_right_day() {
        let utc = FixedOffsetZone::utc();
        // 每周五 18:00。2026-09-15 是星期二。
        let trigger = parse_schedule("0 18 * * FRI", &TimeZone::utc(), NOW, &utc).unwrap();
        let next = next_occurrence(&trigger, NOW, &utc).unwrap().unwrap();
        assert_eq!(next, datetime!(2026-09-18 18:00:00 UTC));
        assert_eq!(next.weekday(), time::Weekday::Friday);
    }

    #[test]
    fn a_one_shot_has_no_next_occurrence_after_it_fires() {
        let utc = FixedOffsetZone::utc();
        let trigger = parse_schedule("@at 2026-09-16 09:00", &TimeZone::utc(), NOW, &utc).unwrap();
        let Trigger::At { at } = trigger.clone() else {
            panic!()
        };
        assert_eq!(next_occurrence(&trigger, NOW, &utc).unwrap(), Some(at));
        assert_eq!(next_occurrence(&trigger, at, &utc).unwrap(), None);
    }

    #[test]
    fn a_job_is_only_due_when_active_and_its_slot_has_come() {
        let zone = shanghai_zone();
        let mut job = CronJob {
            id: CronJobId::from_raw("job-1"),
            name: "morning-summary".into(),
            version: 1,
            trigger: parse_schedule("0 9 * * *", &shanghai(), NOW, &zone).unwrap(),
            prompt: "整理今天的动态".into(),
            workdir: None,
            status: JobStatus::Active,
            overlap: OverlapPolicy::Skip,
            model: None,
            effort: None,
            skills: vec![],
            max_rounds: None,
            next_run_at: None,
            last_error: None,
        };
        job.advance(NOW, &zone).unwrap();
        let slot = job.next_run_at.unwrap();
        assert!(!job.is_due(NOW));
        assert!(job.is_due(slot));

        job.status = JobStatus::Paused;
        assert!(!job.is_due(slot));
    }

    #[test]
    fn a_job_whose_zone_cannot_be_resolved_says_so_instead_of_going_quiet() {
        /// 一个什么区都不认识的 resolver。
        struct NoZones;
        impl ZoneResolver for NoZones {
            fn resolve(
                &self,
                zone: &str,
                _local: PrimitiveDateTime,
            ) -> Result<ZoneResolution, ZoneError> {
                Err(ZoneError::UnknownZone(zone.into()))
            }
            fn offset_at(
                &self,
                zone: &str,
                _instant: OffsetDateTime,
            ) -> Result<UtcOffset, ZoneError> {
                Err(ZoneError::UnknownZone(zone.into()))
            }
        }

        let mut job = CronJob {
            id: CronJobId::from_raw("job-1"),
            name: "x".into(),
            version: 1,
            trigger: Trigger::Cron {
                expr: "0 9 * * *".into(),
                tz: TimeZone::new("Mars/Olympus"),
            },
            prompt: String::new(),
            workdir: None,
            status: JobStatus::Active,
            overlap: OverlapPolicy::Skip,
            model: None,
            effort: None,
            skills: vec![],
            max_rounds: None,
            next_run_at: Some(NOW),
            last_error: None,
        };
        assert!(job.advance(NOW, &NoZones).is_err());
        assert!(job.next_run_at.is_none());
        assert!(job.last_error.unwrap().contains("Mars/Olympus"));

        // 创建时就拒绝，而不是等到 03:00 才发现。
        assert!(matches!(
            parse_schedule("0 9 * * *", &TimeZone::new("Mars/Olympus"), NOW, &NoZones),
            Err(ScheduleError::Zone(ZoneError::UnknownZone(_)))
        ));
    }

    #[test]
    fn a_time_zone_serializes_as_its_iana_name() {
        let tz = shanghai();
        assert_eq!(serde_json::to_string(&tz).unwrap(), "\"Asia/Shanghai\"");
        assert_eq!(
            serde_json::from_str::<TimeZone>("\"Asia/Shanghai\"").unwrap(),
            tz
        );
    }

    #[test]
    fn a_fixed_offset_zone_never_reports_a_gap_or_an_overlap() {
        let zone = FixedOffsetZone::new(offset!(+8));
        assert_eq!(
            zone.resolve("whatever", datetime!(2026-03-29 02:30:00)),
            Ok(ZoneResolution::Single(offset!(+8)))
        );
    }
}
