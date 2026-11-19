//! 外部记忆提供方的接缝（`docs/memory.md` 附录 G）。
//!
//! 只换热路径上能换的那一段：**线索的召回**、**已注入条目的现状核对**、**交给它一批证据**。召回之后的一切——
//! 约定组、事项组、门槛之后的分层、`top_k`、预算、渲染、失效改写——仍是核心的，提供方只交回 [`ProviderHit`]，
//! 核心把它包成一条只读的线索（[`external_item`]）走同一条路。证据也是核心挑好的：会话空闲后同一份提取窗口
//! （只有不是 cron / 任务会话 / `memory = off` 的 Run），注入的记忆正文本来就不在里面。
//!
//! 确认、分类、遗忘、整理、索引这些管理操作只对内置存储有意义，不在这里；记忆页要看它存了什么，只有一个只读的
//! [`MemoryProvider::list`]。

use async_trait::async_trait;
use komo_kernel::types::ids::{MemoryId, SessionId};
use komo_kernel::types::memory::{
    Confirmation, ExtractionMetadata, MemoryApplication, MemoryItem, MemoryKind, MemoryScope,
    MemoryState, MemoryUsage, Provenance,
};
use komo_kernel::types::model::EffortSetting;
use time::OffsetDateTime;

pub use komo_kernel::protocol::config::ProviderMode;

/// 提供方交回的一条。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderHit {
    /// 提供方自己的 id（不带前缀；核心加 `<name>:`）。
    pub id: String,
    pub text: String,
    /// 它写入时归在哪个作用域；读不出来按 [`MemoryScope::Personal`]。
    pub scope: MemoryScope,
    pub score: Option<f64>,
}

/// 清单里的一条（记忆页只读展示，附录 G.9）。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderListed {
    pub id: String,
    pub text: String,
    /// 提供方自己的分类（Hindsight 的 `world` / `experience` / `observation`），原样给界面。
    pub kind: Option<String>,
    pub scope: MemoryScope,
    /// 它记下的事发生在什么时候；读不出来是 `None`。
    pub at: Option<OffsetDateTime>,
}

/// 清单的一页。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProviderPage {
    pub items: Vec<ProviderListed>,
    /// 符合条件的总数（不只这一页）。
    pub total: u64,
}

/// [`crate::MemoryManager::provider_list`] 的结果：装的是哪家、怎么用，以及这一页（问不通如实给出）。
#[derive(Debug, Clone)]
pub struct Listing {
    pub provider: String,
    pub mode: ProviderMode,
    pub page: Result<ProviderPage, ProviderError>,
}

/// 交给提供方的一批证据：一个会话空闲后的一段提取窗口。
#[derive(Debug, Clone)]
pub struct EvidenceBatch<'a> {
    /// 幂等键：同一段窗口重领（提取失败放回 pending）时它不变，提供方按它覆盖而不是再存一份。
    pub key: String,
    pub session: &'a SessionId,
    /// 这批证据归哪个作用域：`memory = private` 的 Agent 是 `agent:<id>`，其余是 Personal。
    pub scope: MemoryScope,
    /// 渲染好的窗口正文（与给记忆模型看的同一份）。
    pub text: &'a str,
    pub at: OffsetDateTime,
}

/// 提供方的失败。只记日志与召回诊断，不拦这一轮。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ProviderError(pub String);

#[async_trait]
pub trait MemoryProvider: Send + Sync {
    /// 短名，进条目 id 前缀与诊断（`hindsight`）。
    fn name(&self) -> &str;

    /// 按一句查询召回，至多 `limit` 条，只在 `scopes` 放行的作用域里（空 = 除 `agent:*` 之外的全部，与
    /// [`komo_kernel::types::memory::scope_allowed`] 同义）。
    async fn recall(
        &self,
        text: &str,
        scopes: &[MemoryScope],
        limit: u32,
    ) -> Result<Vec<ProviderHit>, ProviderError>;

    /// 这些 id 现在还有效的那些，带现在的正文。删掉的、作废的不在结果里。
    async fn current(&self, ids: &[String]) -> Result<Vec<ProviderHit>, ProviderError>;

    /// 交一批证据，由提供方自己提取与存。
    async fn ingest(&self, batch: &EvidenceBatch<'_>) -> Result<(), ProviderError>;

    /// 它现在存着的有效条目，一页（记忆页只读展示）。`query` 非空时按正文筛；不分作用域——看的是操作者。
    async fn list(
        &self,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> Result<ProviderPage, ProviderError>;
}

/// 装上的提供方与它的用法。
#[derive(Clone)]
pub struct Installed {
    pub provider: std::sync::Arc<dyn MemoryProvider>,
    pub mode: ProviderMode,
}

/// 这条记忆是不是外部提供方的（id 带 `<name>:` 前缀）；是就交回提供方自己的 id。
pub(crate) fn external_id<'a>(name: &str, id: &'a MemoryId) -> Option<&'a str> {
    id.as_str()
        .strip_prefix(name)
        .and_then(|rest| rest.strip_prefix(':'))
}

/// 外部的一条包成核心认得的线索：**只读的 reference**——不是约定（约定要用户原话作证，§9.2）、没有归属、
/// 不进候选生命周期。`revision` 取正文的哈希：提供方改了正文，旧注入就按"版本变了"失效（§9.4）。
pub(crate) fn external_item(name: &str, hit: ProviderHit, now: OffsetDateTime) -> MemoryItem {
    MemoryItem {
        id: MemoryId::from_raw(format!("{name}:{}", hit.id)),
        revision: revision_of(&hit.text),
        content: hit.text,
        kind: MemoryKind::Fact,
        scope: hit.scope,
        provenance: Provenance::ModelInference,
        confirmation: Confirmation::Unconfirmed,
        state: MemoryState::Active,
        evidence: Vec::new(),
        observed_at: now,
        valid_until: None,
        created_at: now,
        updated_at: now,
        extraction: ExtractionMetadata {
            model: name.to_string(),
            effort: EffortSetting::ProviderDefault,
            prompt_version: "external".into(),
            source_cursor: None,
        },
        usage: MemoryUsage::default(),
        supersedes: None,
        superseded_by: None,
        application: Some(MemoryApplication::Reference),
        applies_when: None,
        belongs_to: None,
    }
}

/// 正文的稳定哈希（FNV-1a，取低 32 位；0 留给"没有版本"，避开它）。
fn revision_of(text: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in text.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_external_item_is_a_read_only_clue_whose_revision_follows_its_text() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let hit = |text: &str| ProviderHit {
            id: "abc".into(),
            text: text.into(),
            scope: MemoryScope::Personal,
            score: Some(0.5),
        };
        let item = external_item("hindsight", hit("空调设 26 度"), now);
        assert_eq!(item.id.as_str(), "hindsight:abc");
        assert_eq!(item.application, Some(MemoryApplication::Reference));
        assert!(!item.is_convention());
        assert!(item.is_recallable_at(now));
        assert_eq!(
            item.revision,
            external_item("hindsight", hit("空调设 26 度"), now).revision
        );
        assert_ne!(
            item.revision,
            external_item("hindsight", hit("空调设 25 度"), now).revision
        );
        assert_eq!(external_id("hindsight", &item.id), Some("abc"));
        assert_eq!(external_id("hindsight", &MemoryId::from_raw("m-1")), None);
    }
}
