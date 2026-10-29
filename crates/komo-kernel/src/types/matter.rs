//! 目标与领域（`docs/goals.md` §3、§4）：值类型与状态机。
//!
//! 两种事项一张表，`kind` 区分：目标（goal）有结果、会结束；领域（area）没有终点、要维持
//! 标准。状态怎么走、哪些字段随之改、什么时候升版，全在 [`transition`] 这一个纯函数里，
//! 仓储与 Gateway 只负责读写与证据。
//!
//! ```text
//! goal:  active ⇄ waiting
//!           ⇅        ↘
//!         parked  →  done / dropped  →（reopen）active
//! area:  active → retired →（reopen）active
//! ```
//!
//! 超期不是状态，是读出来的（[`Matter::is_overdue`]）：截止日期过了不自动结案（§2）。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
// 渲染函数（`komo-agent`，不依赖 `time`）要在签名里写出日期类型。
pub use time::Date;
use time::OffsetDateTime;

use super::ids::MatterId;
use super::memory::Evidence;

// `YYYY-MM-DD`：截止日期在线上与库里的写法（只有日期，不带时刻）。
time::serde::format_description!(
    pub iso_date,
    Date,
    "[year]-[month]-[day]"
);

/// 标题的上限（字符数，§3）：一句能独立看懂的话。
pub const MAX_TITLE_CHARS: usize = 80;

/// 解析 `YYYY-MM-DD`。
pub fn parse_due(raw: &str) -> Result<Date, String> {
    let format = time::macros::format_description!("[year]-[month]-[day]");
    Date::parse(raw.trim(), &format)
        .map_err(|_| format!("截止日期要写成 YYYY-MM-DD，收到：{}", raw.trim()))
}

/// 事项的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatterKind {
    /// 目标：有结果、会结束，可带截止日期。
    Goal,
    /// 领域：没有终点、要维持标准的职责。
    Area,
}

impl MatterKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MatterKind::Goal => "goal",
            MatterKind::Area => "area",
        }
    }

    /// 给人看的叫法。
    pub fn label(self) -> &'static str {
        match self {
            MatterKind::Goal => "目标",
            MatterKind::Area => "领域",
        }
    }
}

impl fmt::Display for MatterKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MatterKind {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim() {
            "goal" => Ok(MatterKind::Goal),
            "area" => Ok(MatterKind::Area),
            other => Err(format!("kind 要写成 goal / area，收到：{other}")),
        }
    }
}

/// 事项的状态。goal 用前五个，area 用 `active` 与 `retired`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatterState {
    Active,
    /// 只给 goal：在等别人，`waiting_for` 写着等谁、等什么。
    Waiting,
    /// 只给 goal：搁置（GTD 的将来 / 也许）。
    Parked,
    Done,
    Dropped,
    /// 只给 area：不再管。
    Retired,
}

impl MatterState {
    pub fn as_str(self) -> &'static str {
        match self {
            MatterState::Active => "active",
            MatterState::Waiting => "waiting",
            MatterState::Parked => "parked",
            MatterState::Done => "done",
            MatterState::Dropped => "dropped",
            MatterState::Retired => "retired",
        }
    }

    /// 结案了：`done` / `dropped` / `retired`。
    pub fn is_closed(self) -> bool {
        matches!(
            self,
            MatterState::Done | MatterState::Dropped | MatterState::Retired
        )
    }
}

impl fmt::Display for MatterState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MatterState {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim() {
            "active" => Ok(MatterState::Active),
            "waiting" => Ok(MatterState::Waiting),
            "parked" => Ok(MatterState::Parked),
            "done" => Ok(MatterState::Done),
            "dropped" => Ok(MatterState::Dropped),
            "retired" => Ok(MatterState::Retired),
            other => Err(format!("不认识的事项状态：{other}")),
        }
    }
}

/// 一个目标或领域（§3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Matter {
    pub id: MatterId,
    /// 任何会进注入行的字段变了都升一版（注入按 `id@revision` 去重与失效，P1b）。
    pub revision: u32,
    pub kind: MatterKind,
    pub title: String,
    pub state: MatterState,
    /// 只给 goal：下一步。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// 只给 goal：在等谁、等什么；`waiting` 时必有，离开 `waiting` 时清空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    /// 只给 goal：截止日期。
    #[serde(
        default,
        with = "iso_date::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub due: Option<Date>,
    /// 只给 area：要维持的标准。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standard: Option<String>,
    /// 改承诺的那几次操作的用户原话证据，追加不改（§4「谁能改什么」）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// 进 `done` / `dropped` / `retired` 时写，重开时清空。
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub closed_at: Option<OffsetDateTime>,
}

impl Matter {
    /// 超期（§3）：未结案的 goal、`due` 早于 `today`（Gateway 本地时区的今天）。
    pub fn is_overdue(&self, today: Date) -> bool {
        self.kind == MatterKind::Goal
            && !self.state.is_closed()
            && self.due.is_some_and(|due| due < today)
    }
}

/// 召回块里展示着某个事项的哪一版（`id@revision`，`docs/goals.md` §5）。与记忆的
/// [`MemoryUse`](super::turn::MemoryUse) 分开存：两种 id 不会撞，但去重与失效各查各的仓储。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatterUse {
    pub matter: MatterId,
    pub revision: u32,
}

impl Matter {
    /// 这一版此刻该不该进召回块（§5）：goal 的 `active` / `waiting`，area 的 `active`。
    pub fn is_injectable(&self) -> bool {
        match self.kind {
            MatterKind::Goal => matches!(self.state, MatterState::Active | MatterState::Waiting),
            MatterKind::Area => self.state == MatterState::Active,
        }
    }

    pub fn as_use(&self) -> MatterUse {
        MatterUse {
            matter: self.id.clone(),
            revision: self.revision,
        }
    }
}

/// 新建一个事项要的东西。文字已由调用方规整（去空白、查长度与凭证）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMatter {
    pub kind: MatterKind,
    pub title: String,
    pub next: Option<String>,
    pub due: Option<Date>,
    pub standard: Option<String>,
}

impl NewMatter {
    /// 造出 `active`、revision 1 的那一条。种类不对的字段（领域带截止日期、目标带标准）报错。
    pub fn build(self, id: MatterId, now: OffsetDateTime) -> Result<Matter, MatterError> {
        match self.kind {
            MatterKind::Goal if self.standard.is_some() => {
                return Err(MatterError::Invalid("标准只给领域；目标用下一步".into()));
            }
            MatterKind::Area if self.next.is_some() => {
                return Err(MatterError::Invalid("下一步只给目标；领域用标准".into()));
            }
            MatterKind::Area if self.due.is_some() => {
                return Err(MatterError::Invalid(
                    "截止日期只给目标：领域没有终点".into(),
                ));
            }
            _ => {}
        }
        Ok(Matter {
            id,
            revision: 1,
            kind: self.kind,
            title: self.title,
            state: MatterState::Active,
            next: self.next,
            waiting_for: None,
            due: self.due,
            standard: self.standard,
            evidence: Vec::new(),
            created_at: now,
            updated_at: now,
            closed_at: None,
        })
    }
}

/// `POST /v1/matters/{id}/{action}` 的 action：§4 那张表的动词。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatterVerb {
    Next,
    Wait,
    Resume,
    Park,
    Done,
    Drop,
    Retire,
    Reopen,
    Edit,
}

impl MatterVerb {
    pub const ALL: [MatterVerb; 9] = [
        MatterVerb::Next,
        MatterVerb::Wait,
        MatterVerb::Resume,
        MatterVerb::Park,
        MatterVerb::Done,
        MatterVerb::Drop,
        MatterVerb::Retire,
        MatterVerb::Reopen,
        MatterVerb::Edit,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            MatterVerb::Next => "next",
            MatterVerb::Wait => "wait",
            MatterVerb::Resume => "resume",
            MatterVerb::Park => "park",
            MatterVerb::Done => "done",
            MatterVerb::Drop => "drop",
            MatterVerb::Retire => "retire",
            MatterVerb::Reopen => "reopen",
            MatterVerb::Edit => "edit",
        }
    }

    /// 改的是"用户要做 / 要管什么"（§4「谁能改什么」）：在 Run 里要有用户原话作证据。
    /// next / wait / resume 是推进的进度，Agent 自己的工作，不要求原话。
    pub fn is_commitment(self) -> bool {
        !matches!(
            self,
            MatterVerb::Next | MatterVerb::Wait | MatterVerb::Resume
        )
    }
}

impl fmt::Display for MatterVerb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MatterVerb {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        MatterVerb::ALL
            .into_iter()
            .find(|verb| verb.as_str() == raw.trim())
            .ok_or_else(|| {
                format!(
                    "不认识的操作 `{}`：支持 {}",
                    raw.trim(),
                    MatterVerb::ALL.map(MatterVerb::as_str).join(" / ")
                )
            })
    }
}

/// 一次改动，字段已规整。`None` 在可空字段上是"不改"，`Some(None)` 是"清空"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatterChange {
    /// 改下一步；`None` = 清空。
    Next(Option<String>),
    /// 转 `waiting`（或改等的东西）。
    Wait(String),
    /// 回 `active`，顺带可改下一步。
    Resume {
        next: Option<String>,
    },
    Park,
    Done,
    Drop,
    Retire,
    Reopen,
    Edit {
        title: Option<String>,
        due: Option<Option<Date>>,
        standard: Option<Option<String>>,
    },
}

impl MatterChange {
    pub fn verb(&self) -> MatterVerb {
        match self {
            MatterChange::Next(_) => MatterVerb::Next,
            MatterChange::Wait(_) => MatterVerb::Wait,
            MatterChange::Resume { .. } => MatterVerb::Resume,
            MatterChange::Park => MatterVerb::Park,
            MatterChange::Done => MatterVerb::Done,
            MatterChange::Drop => MatterVerb::Drop,
            MatterChange::Retire => MatterVerb::Retire,
            MatterChange::Reopen => MatterVerb::Reopen,
            MatterChange::Edit { .. } => MatterVerb::Edit,
        }
    }
}

/// 状态机拒绝的改动。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MatterError {
    #[error("{kind_label} {id} 没有 {verb} 这个操作（{verb} 只给{other}）")]
    WrongKind {
        id: MatterId,
        kind_label: &'static str,
        verb: MatterVerb,
        other: &'static str,
    },
    #[error("{kind_label} {id} 现在是 {state}，不能 {verb}（允许的起点：{allowed}）")]
    NotAllowed {
        id: MatterId,
        kind_label: &'static str,
        state: MatterState,
        verb: MatterVerb,
        allowed: String,
    },
    #[error("{0}")]
    Invalid(String),
}

/// 一个动词在一种事项上：允许的起点，以及要去的状态（`None` = 不改状态）。
fn rule(
    kind: MatterKind,
    verb: MatterVerb,
) -> Option<(&'static [MatterState], Option<MatterState>)> {
    use MatterState::*;
    Some(match (kind, verb) {
        (MatterKind::Goal, MatterVerb::Next) => (&[Active, Waiting], None),
        (MatterKind::Goal, MatterVerb::Wait) => (&[Active, Waiting], Some(Waiting)),
        (MatterKind::Goal, MatterVerb::Resume) => (&[Waiting, Parked], Some(Active)),
        (MatterKind::Goal, MatterVerb::Park) => (&[Active, Waiting], Some(Parked)),
        (MatterKind::Goal, MatterVerb::Done) => (&[Active, Waiting, Parked], Some(Done)),
        (MatterKind::Goal, MatterVerb::Drop) => (&[Active, Waiting, Parked], Some(Dropped)),
        (MatterKind::Goal, MatterVerb::Reopen) => (&[Done, Dropped], Some(Active)),
        (MatterKind::Goal, MatterVerb::Edit) => (&[Active, Waiting, Parked], None),
        (MatterKind::Area, MatterVerb::Edit) => (&[Active], None),
        (MatterKind::Area, MatterVerb::Retire) => (&[Active], Some(Retired)),
        (MatterKind::Area, MatterVerb::Reopen) => (&[Retired], Some(Active)),
        _ => return None,
    })
}

/// 把一次改动施加到一个事项上（§4）。
///
/// - 种类不对的动词（对领域 `park`）、状态不允许的起点（对 `done` 的目标 `next`）报错，
///   错误里写着当前状态。
/// - 已经在目标状态上（对 `parked` 再 `park`）不算起点不对：状态这一半是空操作，其余字段照改。
/// - 什么都没变返回 `None`：不升版、不动 `updated_at`。
/// - 变了就升一版。离开 `waiting` 清空 `waiting_for`；进结案写 `closed_at`，重开清空。
pub fn transition(
    matter: &Matter,
    change: &MatterChange,
    now: OffsetDateTime,
) -> Result<Option<Matter>, MatterError> {
    let verb = change.verb();
    let Some((allowed, target)) = rule(matter.kind, verb) else {
        let other = match matter.kind {
            MatterKind::Goal => MatterKind::Area.label(),
            MatterKind::Area => MatterKind::Goal.label(),
        };
        return Err(MatterError::WrongKind {
            id: matter.id.clone(),
            kind_label: matter.kind.label(),
            verb,
            other,
        });
    };
    let already_there = target == Some(matter.state);
    if !already_there && !allowed.contains(&matter.state) {
        return Err(MatterError::NotAllowed {
            id: matter.id.clone(),
            kind_label: matter.kind.label(),
            state: matter.state,
            verb,
            allowed: allowed
                .iter()
                .map(|state| state.as_str())
                .collect::<Vec<_>>()
                .join(" / "),
        });
    }

    let mut next = matter.clone();
    if let Some(target) = target {
        next.state = target;
    }
    match change {
        MatterChange::Next(text) => next.next = text.clone(),
        MatterChange::Wait(text) => next.waiting_for = Some(text.clone()),
        MatterChange::Resume { next: Some(text) } => next.next = Some(text.clone()),
        MatterChange::Edit {
            title,
            due,
            standard,
        } => {
            if title.is_none() && due.is_none() && standard.is_none() {
                return Err(MatterError::Invalid(
                    "edit 至少要改一样：标题、截止日期或标准".into(),
                ));
            }
            match matter.kind {
                MatterKind::Goal if standard.is_some() => {
                    return Err(MatterError::Invalid("标准只给领域；目标用下一步".into()));
                }
                MatterKind::Area if due.is_some() => {
                    return Err(MatterError::Invalid(
                        "截止日期只给目标：领域没有终点".into(),
                    ));
                }
                _ => {}
            }
            if let Some(title) = title {
                next.title = title.clone();
            }
            if let Some(due) = due {
                next.due = *due;
            }
            if let Some(standard) = standard {
                next.standard = standard.clone();
            }
        }
        MatterChange::Resume { next: None }
        | MatterChange::Park
        | MatterChange::Done
        | MatterChange::Drop
        | MatterChange::Retire
        | MatterChange::Reopen => {}
    }

    if next.state != MatterState::Waiting {
        next.waiting_for = None;
    }
    if !next.state.is_closed() {
        next.closed_at = None;
    } else if !matter.state.is_closed() {
        next.closed_at = Some(now);
    }

    if next == *matter {
        return Ok(None);
    }
    next.revision = matter.revision + 1;
    next.updated_at = now;
    Ok(Some(next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{date, datetime};

    const T0: OffsetDateTime = datetime!(2026-10-29 08:00 UTC);
    const T1: OffsetDateTime = datetime!(2026-10-29 09:00 UTC);

    fn goal(state: MatterState) -> Matter {
        let mut matter = NewMatter {
            kind: MatterKind::Goal,
            title: "这周把 owl 的发布观察做完".into(),
            next: Some("看 AB 组的 5xx".into()),
            due: Some(date!(2026 - 11 - 01)),
            standard: None,
        }
        .build(MatterId::from_raw("g-1"), T0)
        .unwrap();
        matter.state = state;
        if state == MatterState::Waiting {
            matter.waiting_for = Some("SRE 审批".into());
        }
        if state.is_closed() {
            matter.closed_at = Some(T0);
        }
        matter
    }

    fn area(state: MatterState) -> Matter {
        let mut matter = NewMatter {
            kind: MatterKind::Area,
            title: "告警值班轮换".into(),
            next: None,
            due: None,
            standard: Some("每周一前完成".into()),
        }
        .build(MatterId::from_raw("a-1"), T0)
        .unwrap();
        matter.state = state;
        if state.is_closed() {
            matter.closed_at = Some(T0);
        }
        matter
    }

    fn change_for(verb: MatterVerb) -> MatterChange {
        match verb {
            MatterVerb::Next => MatterChange::Next(Some("新的下一步".into())),
            MatterVerb::Wait => MatterChange::Wait("等回复".into()),
            MatterVerb::Resume => MatterChange::Resume { next: None },
            MatterVerb::Park => MatterChange::Park,
            MatterVerb::Done => MatterChange::Done,
            MatterVerb::Drop => MatterChange::Drop,
            MatterVerb::Retire => MatterChange::Retire,
            MatterVerb::Reopen => MatterChange::Reopen,
            MatterVerb::Edit => MatterChange::Edit {
                title: Some("改过的标题".into()),
                due: None,
                standard: None,
            },
        }
    }

    const GOAL_STATES: [MatterState; 5] = [
        MatterState::Active,
        MatterState::Waiting,
        MatterState::Parked,
        MatterState::Done,
        MatterState::Dropped,
    ];

    /// §4 那张表逐格：每个目标动词 × 每个目标状态，允许的走通、到对的状态，不允许的报错
    /// 并写出当前状态。已在目标状态上的是空操作。
    #[test]
    fn every_goal_transition_follows_the_table() {
        use MatterState::*;
        let table: &[(MatterVerb, &[MatterState], Option<MatterState>)] = &[
            (MatterVerb::Next, &[Active, Waiting], None),
            (MatterVerb::Wait, &[Active, Waiting], Some(Waiting)),
            (MatterVerb::Resume, &[Waiting, Parked], Some(Active)),
            (MatterVerb::Park, &[Active, Waiting], Some(Parked)),
            (MatterVerb::Done, &[Active, Waiting, Parked], Some(Done)),
            (MatterVerb::Drop, &[Active, Waiting, Parked], Some(Dropped)),
            (MatterVerb::Reopen, &[Done, Dropped], Some(Active)),
            (MatterVerb::Edit, &[Active, Waiting, Parked], None),
        ];
        for (verb, allowed, target) in table {
            for state in GOAL_STATES {
                let before = goal(state);
                let result = transition(&before, &change_for(*verb), T1);
                if *target == Some(state) {
                    // 已经在那儿：状态这一半是空操作（wait 例外：改了等的东西）。
                    let after = result.unwrap_or_else(|e| panic!("{verb} 于 {state}：{e}"));
                    if *verb == MatterVerb::Wait {
                        assert_eq!(after.unwrap().waiting_for.as_deref(), Some("等回复"));
                    } else {
                        assert_eq!(after, None, "{verb} 于 {state} 应是空操作");
                    }
                } else if allowed.contains(&state) {
                    let after = result
                        .unwrap_or_else(|e| panic!("{verb} 于 {state}：{e}"))
                        .unwrap_or_else(|| panic!("{verb} 于 {state} 应有改动"));
                    assert_eq!(after.state, target.unwrap_or(state), "{verb} 于 {state}");
                    assert_eq!(after.revision, 2);
                    assert_eq!(after.updated_at, T1);
                } else {
                    let error = result.expect_err(&format!("{verb} 于 {state} 应被拒"));
                    assert!(
                        matches!(error, MatterError::NotAllowed { state: s, .. } if s == state),
                        "{error}"
                    );
                    assert!(error.to_string().contains(state.as_str()), "{error}");
                }
            }
        }
    }

    #[test]
    fn every_area_transition_follows_the_table() {
        use MatterState::*;
        let edit = change_for(MatterVerb::Edit);
        assert_eq!(
            transition(&area(Active), &edit, T1).unwrap().unwrap().title,
            "改过的标题"
        );
        assert!(matches!(
            transition(&area(Retired), &edit, T1),
            Err(MatterError::NotAllowed { state: Retired, .. })
        ));
        let retired = transition(&area(Active), &MatterChange::Retire, T1)
            .unwrap()
            .unwrap();
        assert_eq!(retired.state, Retired);
        assert_eq!(retired.closed_at, Some(T1));
        assert_eq!(
            transition(&area(Retired), &MatterChange::Retire, T1),
            Ok(None)
        );
        let reopened = transition(&area(Retired), &MatterChange::Reopen, T1)
            .unwrap()
            .unwrap();
        assert_eq!(reopened.state, Active);
        assert_eq!(reopened.closed_at, None);
        assert_eq!(
            transition(&area(Active), &MatterChange::Reopen, T1),
            Ok(None)
        );
    }

    /// 目标的动词不给领域，领域的不给目标。
    #[test]
    fn a_verb_of_the_other_kind_is_refused() {
        for verb in [
            MatterVerb::Next,
            MatterVerb::Wait,
            MatterVerb::Resume,
            MatterVerb::Park,
            MatterVerb::Done,
            MatterVerb::Drop,
        ] {
            assert!(matches!(
                transition(&area(MatterState::Active), &change_for(verb), T1),
                Err(MatterError::WrongKind { .. })
            ));
        }
        assert!(matches!(
            transition(&goal(MatterState::Active), &MatterChange::Retire, T1),
            Err(MatterError::WrongKind { .. })
        ));
    }

    /// 离开 waiting 清空 waiting_for；进结案写 closed_at，重开清空。
    #[test]
    fn leaving_waiting_clears_what_it_waited_for_and_closing_stamps_closed_at() {
        let waiting = goal(MatterState::Waiting);
        for change in [
            MatterChange::Resume { next: None },
            MatterChange::Park,
            MatterChange::Done,
            MatterChange::Drop,
        ] {
            let after = transition(&waiting, &change, T1).unwrap().unwrap();
            assert_eq!(after.waiting_for, None, "{:?}", change);
        }
        let done = transition(&waiting, &MatterChange::Done, T1)
            .unwrap()
            .unwrap();
        assert_eq!(done.closed_at, Some(T1));
        let reopened = transition(&done, &MatterChange::Reopen, T1)
            .unwrap()
            .unwrap();
        assert_eq!(reopened.closed_at, None);
        assert_eq!(reopened.state, MatterState::Active);
        // 结案后再结案同一种：空操作，closed_at 不被改写。
        assert_eq!(
            transition(&goal(MatterState::Done), &MatterChange::Done, T1),
            Ok(None)
        );
    }

    /// resume 带 --next 时一并改下一步；在 active 上带 --next 只改下一步。
    #[test]
    fn resume_can_carry_a_new_next_step() {
        let after = transition(
            &goal(MatterState::Parked),
            &MatterChange::Resume {
                next: Some("约 SRE 过一遍".into()),
            },
            T1,
        )
        .unwrap()
        .unwrap();
        assert_eq!(after.state, MatterState::Active);
        assert_eq!(after.next.as_deref(), Some("约 SRE 过一遍"));
        let same = transition(
            &goal(MatterState::Active),
            &MatterChange::Resume {
                next: Some("看 AB 组的 5xx".into()),
            },
            T1,
        );
        assert_eq!(same, Ok(None), "下一步没变就什么都没变");
    }

    /// 什么都没变：不升版、不动 updated_at。
    #[test]
    fn an_unchanged_result_is_a_no_op() {
        let active = goal(MatterState::Active);
        assert_eq!(
            transition(
                &active,
                &MatterChange::Next(Some("看 AB 组的 5xx".into())),
                T1
            ),
            Ok(None)
        );
        assert_eq!(
            transition(&goal(MatterState::Parked), &MatterChange::Park, T1),
            Ok(None)
        );
        let cleared = transition(&active, &MatterChange::Next(None), T1)
            .unwrap()
            .unwrap();
        assert_eq!(cleared.next, None);
        assert_eq!(cleared.revision, 2);
    }

    /// 种类不对的字段：新建与 edit 都拒。edit 什么都不带也拒。
    #[test]
    fn kind_specific_fields_are_refused_on_the_wrong_kind() {
        let bad_area = NewMatter {
            kind: MatterKind::Area,
            title: "家里的设备".into(),
            next: None,
            due: Some(date!(2026 - 11 - 01)),
            standard: None,
        };
        assert!(matches!(
            bad_area.build(MatterId::from_raw("a"), T0),
            Err(MatterError::Invalid(_))
        ));
        let bad_goal = NewMatter {
            kind: MatterKind::Goal,
            title: "发布观察".into(),
            next: None,
            due: None,
            standard: Some("每周".into()),
        };
        assert!(bad_goal.build(MatterId::from_raw("g"), T0).is_err());
        assert!(matches!(
            transition(
                &area(MatterState::Active),
                &MatterChange::Edit {
                    title: None,
                    due: Some(None),
                    standard: None
                },
                T1
            ),
            Err(MatterError::Invalid(_))
        ));
        assert!(matches!(
            transition(
                &goal(MatterState::Active),
                &MatterChange::Edit {
                    title: None,
                    due: None,
                    standard: Some(Some("x".into()))
                },
                T1
            ),
            Err(MatterError::Invalid(_))
        ));
        assert!(matches!(
            transition(
                &goal(MatterState::Active),
                &MatterChange::Edit {
                    title: None,
                    due: None,
                    standard: None
                },
                T1
            ),
            Err(MatterError::Invalid(_))
        ));
        // 清掉截止日期。
        let no_due = transition(
            &goal(MatterState::Active),
            &MatterChange::Edit {
                title: None,
                due: Some(None),
                standard: None,
            },
            T1,
        )
        .unwrap()
        .unwrap();
        assert_eq!(no_due.due, None);
    }

    /// 超期：未结案的目标、截止日期早于今天。当天不算；结案的、领域都不算。
    #[test]
    fn overdue_is_read_from_the_due_date_and_today() {
        let today = date!(2026 - 11 - 02);
        assert!(goal(MatterState::Active).is_overdue(today));
        assert!(goal(MatterState::Waiting).is_overdue(today));
        assert!(goal(MatterState::Parked).is_overdue(today));
        assert!(!goal(MatterState::Active).is_overdue(date!(2026 - 11 - 01)));
        assert!(!goal(MatterState::Done).is_overdue(today));
        assert!(!goal(MatterState::Dropped).is_overdue(today));
        let mut no_due = goal(MatterState::Active);
        no_due.due = None;
        assert!(!no_due.is_overdue(today));
        assert!(!area(MatterState::Active).is_overdue(today));
    }

    /// 截止日期在线上是 `YYYY-MM-DD`；整条往返不丢字段。
    #[test]
    fn a_matter_round_trips_through_json_with_an_iso_due_date() {
        let matter = goal(MatterState::Waiting);
        let json = serde_json::to_value(&matter).unwrap();
        assert_eq!(json["due"], "2026-11-01");
        assert_eq!(json["kind"], "goal");
        assert_eq!(json["state"], "waiting");
        let back: Matter = serde_json::from_value(json).unwrap();
        assert_eq!(back, matter);
        assert_eq!(parse_due(" 2026-11-01 "), Ok(date!(2026 - 11 - 01)));
        assert!(parse_due("11/01").is_err());
    }

    #[test]
    fn verbs_parse_and_split_into_commitment_and_progress() {
        for verb in MatterVerb::ALL {
            assert_eq!(verb.as_str().parse::<MatterVerb>(), Ok(verb));
        }
        let progress: Vec<MatterVerb> = MatterVerb::ALL
            .into_iter()
            .filter(|verb| !verb.is_commitment())
            .collect();
        assert_eq!(
            progress,
            vec![MatterVerb::Next, MatterVerb::Wait, MatterVerb::Resume]
        );
        assert!("frobnicate".parse::<MatterVerb>().is_err());
    }
}
