//! 自动记忆的数据模型（§9.2）。
//!
//! 确认状态与来源**分开保存**：把"模型从用户原话整理"写成"用户确认了模型摘要"是这
//! 组类型存在的全部理由。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::ids::{EventId, MemoryId, RunId, Seq, SessionId};
use super::model::{Effort, EffortSetting, Vector};

/// 记忆的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Preference,
    Fact,
    Experience,
}

/// 作用域，绑定当前操作者、稳定项目 ID 或实例 ID。工作路径相似**不**自动意味着同一
/// 项目（§9.2）。
///
/// 在 JSON 里是一个带 `kind` 的对象；在 **query string 里**是一行文本
/// （`personal` / `project:<id>` / `environment:<id>`，见 [`Display`] 与 [`FromStr`]），
/// 因为 `?scope=` 装不下一个对象。两种写法必须能互相还原，所以它们有一个往返测试。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MemoryScope {
    Personal,
    Project { project_id: String },
    Environment { instance_id: String },
}

impl fmt::Display for MemoryScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryScope::Personal => f.write_str("personal"),
            MemoryScope::Project { project_id } => write!(f, "project:{project_id}"),
            MemoryScope::Environment { instance_id } => write!(f, "environment:{instance_id}"),
        }
    }
}

/// 解析作用域文本失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("作用域要写成 personal / project:<id> / environment:<id>，收到：{0}")]
pub struct MemoryScopeParseError(String);

impl FromStr for MemoryScope {
    type Err = MemoryScopeParseError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let trimmed = raw.trim();
        let invalid = || MemoryScopeParseError(raw.to_string());
        match trimmed.split_once(':') {
            None if trimmed == "personal" => Ok(MemoryScope::Personal),
            None => Err(invalid()),
            Some((_, "")) => Err(invalid()),
            Some(("project", id)) => Ok(MemoryScope::Project {
                project_id: id.to_string(),
            }),
            Some(("environment", id)) => Ok(MemoryScope::Environment {
                instance_id: id.to_string(),
            }),
            Some(_) => Err(invalid()),
        }
    }
}

/// 这条内容是谁说的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    UserStatement,
    ToolObservation,
    ModelInference,
}

/// 确认等级。模型返回的 `user_confirmed` 字段没有写入权限（§9.2）——只有操作者的
/// `confirm` 能把它抬起来。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confirmation {
    Unconfirmed,
    UserConfirmed,
}

/// 生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryState {
    /// 模型推断默认落这里，不作为已确认事实注入。
    Candidate,
    Active,
    /// 存在实质冲突，暂停正常召回，等待用户纠正（§9.6）。
    Contested,
    Superseded,
    Forgotten,
}

impl MemoryState {
    /// 可以进入自动召回。`contested` 暂停召回，但**显式 search 仍要能查到**——不然
    /// 用户没法帮着把冲突定下来。
    pub fn is_recallable(self) -> bool {
        matches!(self, MemoryState::Active)
    }
}

/// 证据引用：Session ID + event_id / seq 稳定定位原始消息或工具结果（§8.3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvidenceRef {
    /// 一条 JSONL 事件。
    Event {
        session: SessionId,
        event: EventId,
        seq: Seq,
    },
    /// 一条 Memos 记录（实例 + 记录 ID + 读到时的内容哈希）。
    Memos {
        instance: String,
        record_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_hash: Option<super::digest::ContentHash>,
    },
}

/// 一条证据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub reference: EvidenceRef,
    pub provenance: Provenance,
    #[serde(with = "time::serde::rfc3339")]
    pub observed_at: OffsetDateTime,
    /// 哪一次 Run 的处理提取出来的。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extracted_from_run: Option<RunId>,
}

/// 提取这条记忆时的模型与提示词版本（§9.2）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionMetadata {
    pub model: String,
    pub effort: EffortSetting,
    pub prompt_version: String,
    /// 处理到哪个来源游标。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_cursor: Option<Seq>,
}

impl ExtractionMetadata {
    pub fn new(
        model: impl Into<String>,
        effort: Option<Effort>,
        prompt_version: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            effort: EffortSetting::from_option(effort),
            prompt_version: prompt_version.into(),
            source_cursor: None,
        }
    }
}

/// 使用情况。**只度量使用，不增加真实性**（§9.2）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryUsage {
    #[serde(default)]
    pub count: u64,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_used_at: Option<OffsetDateTime>,
}

/// 这条内容取代了**哪一条记忆的哪个版本**（§9.6）。
///
/// 取代是跨条目的：新主张是一条自己的记忆，旧的那条随后标 [`MemoryState::Superseded`]。
/// 前向链留在新的这一条上，因为读到的人手里拿着的正是它——"它取代了什么"要能就地答
/// 出来，而不是反过来去全表找谁指着自己。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupersededRef {
    pub memory: MemoryId,
    pub revision: u32,
}

/// 一条自动记忆。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: MemoryId,
    /// 递增内容版本；修改产生新版本。confirm / forget 携带预期 revision（§9.6）。
    pub revision: u32,
    pub content: String,
    pub kind: MemoryKind,
    pub scope: MemoryScope,
    pub provenance: Provenance,
    pub confirmation: Confirmation,
    pub state: MemoryState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
    /// 事实的**观察**时间，区别于入库时间。
    #[serde(with = "time::serde::rfc3339")]
    pub observed_at: OffsetDateTime,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub valid_until: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    pub extraction: ExtractionMetadata,
    #[serde(default)]
    pub usage: MemoryUsage,
    /// 取代了哪一条（§9.6）。`None` = 这条不取代任何东西。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<SupersededRef>,
}

impl MemoryItem {
    /// 在 `now` 这一刻能不能进入自动召回：状态可召回，且没过有效期（§9.4）。
    pub fn is_recallable_at(&self, now: OffsetDateTime) -> bool {
        self.state.is_recallable() && self.valid_until.is_none_or(|until| until > now)
    }
}

/// 检索模式（§9.4）。默认 hybrid；keyword / vector 供明确选择与诊断。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalMode {
    #[default]
    Hybrid,
    Keyword,
    Vector,
}

/// 一次检索的请求。
///
/// **`Eq` 不在派生列表里**：`query_vector` 里是 f32，而"两个 NaN 相等"不是这个类型
/// 能签的字。没有谁把它放进 `HashSet`，所以这只是把一句不成立的话去掉。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecallQuery {
    pub text: String,
    pub mode: RetrievalMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<MemoryScope>,
    pub candidate_limit: u32,
    pub top_k: u32,
    pub max_tokens: u32,
    /// 有效期与状态都按这一刻判定。kernel 不读时钟。
    #[serde(with = "time::serde::rfc3339")]
    pub now: OffsetDateTime,
    /// 查询向量（§9.4 的向量臂）。**由 MemoryManager 算好交下来**——仓储不认识
    /// embedding 端点，也不该在一次读事务里发网络请求（§9.5「模型调用期间不持有数据库
    /// 事务」）。`None` + 非 keyword 模式 = 向量臂这一次不可用，实现要如实标 degraded。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_vector: Option<Vector>,
    /// 放行哪些状态。**空 = 只要能自动召回的那些**（active 且没过期，§9.4）。
    ///
    /// 显式列出状态是"明确 search"这条路：`contested` 暂停自动召回，但用户要能查到它，
    /// 否则没人帮得上把冲突定下来（§9.6）。去重与冲突整理也走这一口，它要看见候选。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include_states: Vec<MemoryState>,
}

impl RecallQuery {
    /// 这条记忆能不能出现在**这一次**检索的结果里。
    pub fn admits(&self, item: &MemoryItem) -> bool {
        if self.include_states.is_empty() {
            return item.is_recallable_at(self.now);
        }
        self.include_states.contains(&item.state)
            && item.valid_until.is_none_or(|until| until > self.now)
    }
}

/// 一次检索的结果，带检索元信息——降级要明说（§9.4）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecallResult {
    pub items: Vec<MemoryItem>,
    pub mode: RetrievalMode,
    /// 向量臂不可用，已退化为关键词。
    #[serde(default)]
    pub degraded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<String>,
    /// 向量覆盖率：多少候选条目有当前代次的向量。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_coverage: Option<f32>,
}

/// Run 的记忆处理进度（`runs.memory_work`，§9.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryWork {
    Pending,
    Processing,
    Done,
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn item(state: MemoryState) -> MemoryItem {
        MemoryItem {
            id: MemoryId::from_raw("m-1"),
            revision: 1,
            content: "喜欢深色主题".into(),
            kind: MemoryKind::Preference,
            scope: MemoryScope::Personal,
            provenance: Provenance::UserStatement,
            confirmation: Confirmation::Unconfirmed,
            state,
            evidence: vec![],
            observed_at: datetime!(2026-09-01 00:00:00 UTC),
            valid_until: None,
            created_at: datetime!(2026-09-01 00:00:00 UTC),
            updated_at: datetime!(2026-09-01 00:00:00 UTC),
            extraction: ExtractionMetadata::new("m", None, "v1"),
            usage: MemoryUsage::default(),
            supersedes: None,
        }
    }

    fn any_query(include_states: Vec<MemoryState>) -> RecallQuery {
        RecallQuery {
            text: "深色".into(),
            mode: RetrievalMode::Keyword,
            scopes: vec![],
            candidate_limit: 40,
            top_k: 8,
            max_tokens: 1500,
            now: datetime!(2026-09-15 00:00:00 UTC),
            query_vector: None,
            include_states,
        }
    }

    /// `contested` 暂停自动召回，但**显式列出状态**时要查得到（§9.6）。
    #[test]
    fn an_explicit_state_list_is_what_reaches_a_contested_memory() {
        let contested = item(MemoryState::Contested);
        assert!(!any_query(vec![]).admits(&contested), "自动召回够不着");
        assert!(
            any_query(vec![MemoryState::Contested]).admits(&contested),
            "明确 search 要查得到"
        );
        // 过期仍然是过期——列出状态不等于放行有效期。
        let mut expired = item(MemoryState::Contested);
        expired.valid_until = Some(datetime!(2026-09-10 00:00:00 UTC));
        assert!(!any_query(vec![MemoryState::Contested]).admits(&expired));
    }

    #[test]
    fn only_active_memories_are_recalled_automatically() {
        let now = datetime!(2026-09-15 00:00:00 UTC);
        assert!(item(MemoryState::Active).is_recallable_at(now));
        for state in [
            MemoryState::Candidate,
            MemoryState::Contested,
            MemoryState::Superseded,
            MemoryState::Forgotten,
        ] {
            assert!(!item(state).is_recallable_at(now), "{state:?}");
        }
    }

    #[test]
    fn an_expired_memory_is_not_recalled() {
        let mut m = item(MemoryState::Active);
        m.valid_until = Some(datetime!(2026-09-10 00:00:00 UTC));
        assert!(!m.is_recallable_at(datetime!(2026-09-15 00:00:00 UTC)));
        assert!(m.is_recallable_at(datetime!(2026-09-05 00:00:00 UTC)));
    }

    #[test]
    fn a_scope_round_trips_through_its_query_string_spelling() {
        for scope in [
            MemoryScope::Personal,
            MemoryScope::Project {
                project_id: "komo".into(),
            },
            MemoryScope::Environment {
                instance_id: "nas".into(),
            },
        ] {
            let text = scope.to_string();
            assert_eq!(text.parse::<MemoryScope>().unwrap(), scope, "{text}");
        }
        assert_eq!(
            "  project:komo ".parse::<MemoryScope>().unwrap(),
            MemoryScope::Project {
                project_id: "komo".into()
            }
        );
        // 带冒号的 id 原样留着——项目 ID 是别人给的，不是这里的语法。
        assert_eq!(
            "project:a:b".parse::<MemoryScope>().unwrap(),
            MemoryScope::Project {
                project_id: "a:b".into()
            }
        );
        for bad in [
            "",
            "personal:x",
            "project",
            "project:",
            "team:x",
            "PERSONAL",
        ] {
            assert!(bad.parse::<MemoryScope>().is_err(), "{bad}");
        }
    }

    #[test]
    fn a_stored_memory_without_usage_reads_as_unused() {
        let json = r#"{
            "id":"m-1","revision":1,"content":"x","kind":"fact","scope":{"kind":"personal"},
            "provenance":"user_statement","confirmation":"unconfirmed","state":"active",
            "observed_at":"2026-09-01T00:00:00Z","created_at":"2026-09-01T00:00:00Z",
            "updated_at":"2026-09-01T00:00:00Z",
            "extraction":{"model":"m","effort":{"kind":"provider_default"},"prompt_version":"v1"}
        }"#;
        let m: MemoryItem = serde_json::from_str(json).unwrap();
        assert_eq!(m.usage.count, 0);
        assert!(m.evidence.is_empty());
    }
}
