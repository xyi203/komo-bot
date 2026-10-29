//! 目标与领域的线格式（`docs/goals.md` §4）：`GET/POST /v1/matters`、`GET /v1/matters/{id}`、
//! `POST /v1/matters/{id}/{action}`。
//!
//! 带 `session` + `run` 的写 = Agent 在 Run 里推进（bash 子进程带着 `KOMO_SESSION` /
//! `KOMO_RUN`）：改承诺的操作要这个 Run 里有用户原话；都不带 = 操作者在终端。只给一个报错，
//! 同 `memory add`。

use serde::{Deserialize, Serialize};
use time::Date;

use crate::types::ids::{MemoryId, RequestKey, RunId, SessionId};
use crate::types::matter::{Matter, MatterKind, iso_date};

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatterListResponse {
    pub matters: Vec<Matter>,
    #[serde(with = "iso_date")]
    pub today: Date,
}

/// 一条事项，以及读它时的今天（同 [`MatterListResponse::today`]）。所有写接口也回它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatterDetail {
    pub matter: Matter,
    #[serde(with = "iso_date")]
    pub today: Date,
    /// 只在这次操作是结案（done / drop / retire）或重开时带：这件事下面的记忆怎么处理了
    /// （`docs/goals.md` §6）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closure: Option<MatterClosure>,
}

/// 结案时它名下的记忆怎么处理了（`memory.md` §9.6「结案归档」）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatterClosure {
    /// 这次转成 `archived` 的线索条数。
    #[serde(default)]
    pub archived: u32,
    /// 归属它、仍在生效的约定：不因结案悄悄失效，由操作者定去向（`classify --for` 或 `forget`），
    /// 没答之前照常注入。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conventions: Vec<ClosureConvention>,
    /// 这次顺带把归档的线索恢复成 `active` 了几条（只在 reopen 时有）。
    #[serde(default)]
    pub restored: u32,
}

/// 结案时列出的一条约定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosureConvention {
    pub id: MemoryId,
    pub revision: u32,
    pub content: String,
    /// `applies_when`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applies_when: Option<String>,
}

/// `POST /v1/matters`：新建，`active`。`next` / `due` 只给 goal，`standard` 只给 area。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    pub due: Option<Date>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standard: Option<String>,
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
/// | `edit` | `title` / `due` / `clear_due`（goal）/ `standard`（area；空 = 清空），至少一样 |
/// | 其余 | 无 |
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    pub due: Option<Date>,
    /// `--no-due`。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_due: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standard: Option<String>,
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
    }
}
