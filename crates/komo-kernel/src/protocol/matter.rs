//! 目标与领域的线格式（`docs/goals.md` §4）：`GET/POST /v1/matters`、`GET /v1/matters/{id}`、
//! `POST /v1/matters/{id}/{action}`，以及目标页要的 `GET /v1/matters/{id}/runs`（定时的运行历史）、
//! `POST|DELETE /v1/matters/{id}/chat`（新开一段支线并绑定 / 解除，`docs/redesign.md` P22）。
//!
//! 带 `session` + `run` 的写 = Agent 在 Run 里推进（bash 子进程带着 `KOMO_SESSION` /
//! `KOMO_RUN`）：改承诺的操作要这个 Run 里有用户原话；都不带 = 操作者在终端。只给一个报错，
//! 同 `memory add`。

use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};

use crate::cron::{CronFiring, CronJob, FiringStatus, NotifyPolicy, OverlapPolicy};
use crate::protocol::http::{ManualCronRunResponse, MonitorSummary, TaskSummary};
use crate::types::ids::{MemoryId, RequestKey, RunId, SessionId};
use crate::types::matter::{Matter, MatterKind, MatterTrigger, iso_date};
use crate::types::memory::MemoryItem;
use crate::types::model::{Effort, ModelConfig};

/// 一个目标上的定时：配置与运行面（`docs/redesign.md` P8）。`job.id` 就是目标 id。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct MatterSchedule {
    pub job: CronJob,
    /// 最近一次触发（跳过也算）；没有就是还没响过。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<CronFiring>,
}

/// 设置或修改一个目标的定时：创建目标时随 [`MatterAddRequest::schedule`] 给全，之后用
/// `schedule` 动词，**给出的字段才改**。
///
/// `prompt` 与 `command` 二选一（命令直跑，不经模型）；都不给时到点做的事取目标的说明。
/// 时区与 effort 在请求前校验，并指出支持值。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct ScheduleSpec {
    /// 五字段 cron 表达式或 `@at YYYY-MM-DD HH:MM`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    /// IANA 时区名。新建时必给；之后不给就沿用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// 模型目录里的 alias。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// **完整**模型配置覆盖；给了它就整份用它，`model` 被忽略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(type = "unknown"))]
    pub model_config: Option<ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlap: Option<OverlapPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
    /// 触发结果的投递规则。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<NotifyPolicy>,
    /// 暂停 / 恢复：`false` 暂停，`true` 恢复。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// `GET /v1/matters`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatterListQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MatterKind>,
    /// 带上结案的；默认只列未结案的。
    #[serde(default)]
    pub all: bool,
    /// 在 Run 里跑的 CLI 带上自己的会话：`memory = private / off` 的会话被拒（§2）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
}

/// `GET /v1/matters/{id}` 的 query：只有会话（同 [`MatterListQuery::session`]）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatterShowQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
}

/// 清单。`today` 是 Gateway 本地时区的今天：超期按它读（[`Matter::is_overdue`]），客户端
/// 不拿自己的时区再算一遍。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct MatterListResponse {
    pub matters: Vec<Matter>,
    /// 带定时的目标的定时（`job.id` 对得上 `matters[].id`）；没有定时的目标不在里面。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "ts", ts(as = "Option<Vec<MatterSchedule>>"))]
    pub schedules: Vec<MatterSchedule>,
    /// 此刻在等你的目标：目标 id → 问话所在的对话与介入句柄（形状同目标活动的 `waiting`）。没有在等的
    /// 目标不出现；整张表为空时省略这个字段。
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    #[cfg_attr(feature = "ts", ts(as = "Option<std::collections::BTreeMap<String, ActivityWaiting>>"))]
    pub waiting: std::collections::BTreeMap<String, ActivityWaiting>,
    #[serde(with = "iso_date")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub today: Date,
}

/// 一条事项，以及读它时的今天（同 [`MatterListResponse::today`]）。所有写接口也回它。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct MatterDetail {
    pub matter: Matter,
    /// 这个目标的定时（没有就不带）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<MatterSchedule>,
    /// 只在 `run` 动词的回应里带：这次手动触发建出的会话与 Run。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fired: Option<ManualCronRunResponse>,
    /// 目标页要的其余东西（绑定的支线、监听、活动、记忆）：只有 `GET /v1/matters/{id}` 带；
    /// 写接口的回应不带（要的话再读一次）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overview: Option<MatterOverview>,
    #[serde(with = "iso_date")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub today: Date,
    /// done / drop / retire 的回应带：这条结案的事项还挂着的触发器，提醒操作者去留（不自动删，
    /// §8）。其余操作与没有挂着的时候为空。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "ts", ts(as = "Option<Vec<MatterTrigger>>"))]
    pub still_linked: Vec<MatterTrigger>,
    /// 只在这次操作是结案（done / drop / retire）或重开时带：这件事下面的记忆怎么处理了
    /// （`docs/goals.md` §6）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closure: Option<MatterClosure>,
}

/// 目标绑定的支线对话（`docs/redesign.md` P22）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct MatterChat {
    pub session: SessionId,
    pub title: String,
}

/// 一条在等你的事：问话投在哪段对话里、对应哪条介入（那句话上 `data.handle` 就是它）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ActivityWaiting {
    /// 问话所在的对话（绑定了支线是那段支线，否则主对话）。
    pub chat: SessionId,
    /// 介入的句柄。
    pub handle: String,
}

/// 目标活动里的一条：为这个目标做的一次事——定时触发的运行（含手动触发）、以及带着这个目标的键的
/// 任务（`goal.attach`，或定时运行派出的任务）。最新的在前。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct MatterActivity {
    /// 背后的会话：状态、结局、起止时刻、点进去看过程都用它（同任务清单的一行）。
    pub task: TaskSummary,
    /// 这件事此刻在等你（P16）：有，就是「在等你」，点了去 `waiting.chat` 里那句话。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<ActivityWaiting>,
}

/// 目标详情页要的、不属于目标本身的那几块（`GET /v1/matters/{id}`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct MatterOverview {
    /// 绑定的支线对话；没绑定、或那段支线已归档 / 删除时没有（读取时视为未绑定）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<MatterChat>,
    /// 挂着的监听（`matter.triggers` 里的 monitor 对上登记的那些；登记里已经找不到的不在这里，
    /// 触发器引用仍在 `matter.triggers`）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "ts", ts(as = "Option<Vec<MonitorSummary>>"))]
    pub monitors: Vec<MonitorSummary>,
    /// 活动时间线，最新的在前，最多 [`MATTER_ACTIVITY_LIMIT`] 条；更早的定时触发看运行历史。
    /// 「近况」取第一条，不另存。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "ts", ts(as = "Option<Vec<MatterActivity>>"))]
    pub activity: Vec<MatterActivity>,
    /// 为它记的记忆（归属它、还没归档 / 遗忘的），最多 [`MATTER_MEMORY_LIMIT`] 条。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "ts", ts(as = "Option<Vec<MemoryItem>>"))]
    pub memories: Vec<MemoryItem>,
}

/// 详情里带的活动条数上限。
pub const MATTER_ACTIVITY_LIMIT: usize = 20;
/// 详情里带的记忆条数上限。
pub const MATTER_MEMORY_LIMIT: usize = 50;

/// `POST /v1/matters/{id}/chat`（新开一段支线并绑定）与 `DELETE /v1/matters/{id}/chat`（解除）的回应。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct MatterChatResponse {
    /// 现在绑定的支线；解除之后没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<MatterChat>,
    /// 这一次新开了一段（已经绑着活的支线时不再开，原样返回，`false`）。
    #[serde(default)]
    pub created: bool,
}

/// `GET /v1/matters/{id}/runs` 的 query：翻页用 `before`（上一页的 `next_before`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatterRunsQuery {
    /// 一页几条；不给 20，上限 100。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// 只要计划时间早于它的（RFC 3339）。
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub before: Option<OffsetDateTime>,
}

/// 定时触发的一条记录（`cron_firings` 的一行，加上读出来的实际时间与「在等你」）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct MatterRun {
    /// 计划时间。
    #[serde(with = "time::serde::rfc3339")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub scheduled_at: OffsetDateTime,
    /// 实际触发的时刻（建出来的会话的创建时刻）；跳过的、没投成的没有。
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub fired_at: Option<OffsetDateTime>,
    /// 记录里的状态：`queued` / `ok` / `error` / `waiting` / `skipped`。
    pub status: FiringStatus,
    /// `skipped` 的原因或 `error` 的错误。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 触发时定时的版本。
    pub job_version: u64,
    /// 点进去看过程的句柄：会话与 Run。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    /// 此刻停在等你上（同 [`MatterActivity::waiting`]）；`status` 不会变成 `waiting`，这一格是现读的。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<ActivityWaiting>,
}

/// `GET /v1/matters/{id}/runs`：新的在前。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct MatterRunsResponse {
    pub runs: Vec<MatterRun>,
    /// 还有更早的：下一页的 `before`（这一页最后一条的计划时间）；没有更多就缺省。
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub next_before: Option<OffsetDateTime>,
}

/// 结案时它名下的记忆怎么处理了（`memory.md` §9.6「结案归档」）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct MatterClosure {
    /// 这次转成 `archived` 的线索条数。
    #[serde(default)]
    pub archived: u32,
    /// 归属它、仍在生效的约定：不因结案悄悄失效，由操作者定去向（`classify --for` 或 `forget`），
    /// 没答之前照常注入。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "ts", ts(as = "Option<Vec<ClosureConvention>>"))]
    pub conventions: Vec<ClosureConvention>,
    /// 这次顺带把归档的线索恢复成 `active` 了几条（只在 reopen 时有）。
    #[serde(default)]
    pub restored: u32,
}

/// 结案时列出的一条约定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct ClosureConvention {
    pub id: MemoryId,
    pub revision: u32,
    pub content: String,
    /// `applies_when`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applies_when: Option<String>,
}

/// `POST /v1/matters`：新建，`active`。`next` / `due` 只给 goal，`standard` 只给 area。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct MatterAddRequest {
    pub kind: MatterKind,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    #[serde(
        default,
        with = "iso_date::option",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub due: Option<Date>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standard: Option<String>,
    /// 说明。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 同时给它一个定时：要给全（表达式、时区）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ScheduleSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<RequestKey>,
}

/// `POST /v1/matters/{id}/{action}` 的请求体。各动词只认自己的字段，带了别的报错：
///
/// | action | 字段 |
/// |---|---|
/// | `next` | `next`（必填；空 = 清空下一步） |
/// | `wait` | `waiting_for`（必填） |
/// | `resume` | `next`（可选） |
/// | `link` / `unlink` | `trigger`（必填：Cron Job 或 monitor） |
/// | `edit` | `title` / `due` / `clear_due`（goal）/ `standard`（area；空 = 清空）/ `description`（空 = 清空），至少一样 |
/// | `schedule` | `schedule`（[`ScheduleSpec`]，给出的字段才改；还没有定时时要给全） |
/// | `unschedule` / `run` | 无 |
/// | 其余 | 无 |
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct MatterActionRequest {
    /// CLI 知道自己是 `komo goal` 还是 `komo area`：给了就核对，对不上报错（拿领域的 id 跑
    /// `komo goal edit` 不该悄悄改了领域）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MatterKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    #[serde(
        default,
        with = "iso_date::option",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub due: Option<Date>,
    /// `--no-due`。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[cfg_attr(feature = "ts", ts(as = "Option<bool>"))]
    pub clear_due: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standard: Option<String>,
    /// 说明；空串清空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `schedule` 动词的内容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ScheduleSpec>,
    /// `link` / `unlink` 要挂 / 摘的触发器。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<MatterTrigger>,
    /// 预期版本（`--revision N`）；不符报版本冲突。不带就按当前版本改（§4）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<RequestKey>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    #[test]
    fn an_add_request_carries_the_due_date_as_iso_text() {
        let request = MatterAddRequest {
            kind: MatterKind::Goal,
            title: "发布观察".into(),
            next: None,
            due: Some(date!(2026 - 11 - 01)),
            standard: None,
            description: None,
            schedule: None,
            session: None,
            run: None,
            request_key: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["due"], "2026-11-01");
        assert!(json.get("next").is_none());
        let back: MatterAddRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, request);
        // 只给必填字段也读得出来。
        let action: MatterActionRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(action, MatterActionRequest::default());
        // 挂触发器的请求体与没有触发器的回应：旧客户端读得懂，新字段为空时不出现。
        let link = MatterActionRequest {
            trigger: Some(MatterTrigger::parse_monitor("sess-1:#3f2a").unwrap()),
            ..Default::default()
        };
        let json = serde_json::to_value(&link).unwrap();
        assert_eq!(json["trigger"]["kind"], "monitor");
        assert_eq!(
            serde_json::from_value::<MatterActionRequest>(json).unwrap(),
            link
        );
    }
}
