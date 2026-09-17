//! `MemoryRepo`：MemoryManager 的全部状态读写（§9）。
//!
//! **这是允许写 raw SQL 的三个模块之二**：关键词臂要 `instr`，而 `instr` 不在 toasty
//! 的类型化 API 里。原因往上一层：Turso 的 MVCC 下建不出 FTS 索引（`CREATE INDEX …
//! USING fts` 报 `Custom index modules are not supported in MVCC mode`，§8.2 实测），
//! 所以分词挪到**索引时**——`memory_terms.terms` 是首尾带空格的 token 串，查询时每个
//! token 一个 `instr(terms, ' tok ') > 0`，命中数按 IDF 加权（§9.4）。
//!
//! 两条臂都在这里，**融合也在这里**（[`fuse`]）：一次 `recall` 出去一趟数据库，回来的
//! 是已经按 RRF 排好、按作用域与状态筛过的条目。向量由 [`RecallQuery::query_vector`]
//! 交下来——仓储不认识 embedding 端点，也不该在一次读事务里发网络请求（§9.5「模型调用
//! 期间不持有数据库事务」）。
//!
//! 向量臂拿不到查询向量、或者没有生效代次时，`hybrid` 退化为关键词并如实把
//! [`RecallResult::degraded`] 置位、写清原因与覆盖率（§9.4）；`vector` 模式返回空集**并
//! 且**带着 degraded，由上一层翻成"不可用"。把故障解释成"没有相关记忆"是这里唯一不能
//! 说的话。

use async_trait::async_trait;
use komo_kernel::protocol::http::{IndexState, MemoryIndexStatus};
use komo_kernel::traits::{MemoryRepo, RepoError, StoreError};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::{MemoryId, RunId};
use komo_kernel::types::memory::{
    Evidence, MemoryItem, MemoryScope, MemoryState, RecallQuery, RecallResult, RetrievalMode,
};
use komo_kernel::types::model::Vector;
use time::OffsetDateTime;
use toasty::Executor;

#[cfg(test)]
mod bench;
mod catalog;
mod fuse;
mod terms;

use crate::db::{
    BoxFuture, Db, column_i64, decode, encode, from_ts, from_ts_opt, map_toasty, to_ts, to_ts_opt,
};
pub use catalog::{GenerationRecord, IndexCandidate};
pub use fuse::{RRF_K, cosine, decode_vector, encode_vector, reciprocal_rank_fusion};
pub use terms::{lexical_terms, terms_column};

use crate::models::{
    MemoryEvidenceRow, MemoryIndexGenerationRow, MemoryItemRow, MemoryTermsRow, MemoryVectorRow,
};

/// Turso 上的 [`MemoryRepo`]。
#[derive(Debug, Clone)]
pub struct TursoMemoryRepo {
    db: Db,
}

impl TursoMemoryRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// 登记一个索引代次（§9.5）。`active` 为真时它就是"当前查询用哪一代"。
    pub async fn put_generation(
        &self,
        generation: &str,
        space: Option<&komo_kernel::types::model::EmbeddingSpace>,
        state: IndexState,
        active: bool,
    ) -> Result<(), RepoError> {
        let generation = generation.to_string();
        let space = space.cloned();
        let now = OffsetDateTime::now_utc();
        self.db
            .with_write_retry(move |ex| {
                let (generation, space) = (generation.clone(), space.clone());
                Box::pin(async move {
                    let fingerprint = space
                        .as_ref()
                        .map(|s| s.fingerprint().as_str().to_string())
                        .unwrap_or_default();
                    let dimensions = space.as_ref().map(|s| i64::from(s.dimensions)).unwrap_or(0);
                    let encoded = match &space {
                        Some(space) => Some(encode(space)?),
                        None => None,
                    };
                    let state_str = enum_str(&state);
                    if let Some(mut row) = MemoryIndexGenerationRow::filter_by_id(&generation)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    {
                        row.update()
                            .space(encoded)
                            .fingerprint(fingerprint)
                            .state(state_str)
                            .dimensions(dimensions)
                            .active(active)
                            .activated_at(if active { to_ts(now) } else { 0 })
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?;
                        return Ok(());
                    }
                    toasty::create!(MemoryIndexGenerationRow {
                        id: generation,
                        space: encoded,
                        fingerprint,
                        state: state_str,
                        dimensions,
                        indexed: 0_i64,
                        total: 0_i64,
                        errors: "[]".to_string(),
                        active,
                        created_at: to_ts(now),
                        activated_at: if active { to_ts(now) } else { 0 },
                    })
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    Ok(())
                }) as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    /// 当前生效的索引代次。没有配置向量模型时是 `None`。
    pub async fn active_generation(&self) -> Result<Option<String>, RepoError> {
        self.db
            .read(move |ex| {
                Box::pin(async move {
                    let rows = MemoryIndexGenerationRow::filter(
                        MemoryIndexGenerationRow::fields().active().eq(true),
                    )
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    Ok(rows.into_iter().map(|row| row.id).next())
                }) as BoxFuture<'_, Result<Option<String>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }
}

#[async_trait]
impl MemoryRepo for TursoMemoryRepo {
    async fn get(&self, id: &MemoryId) -> Result<Option<MemoryItem>, RepoError> {
        let id = id.to_string();
        self.db
            .read(move |ex| {
                let id = id.clone();
                Box::pin(async move { load_item(ex, &id).await })
                    as BoxFuture<'_, Result<Option<MemoryItem>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn put(
        &self,
        item: MemoryItem,
        expected_revision: Option<u32>,
    ) -> Result<MemoryItem, RepoError> {
        self.db
            .with_write_retry(move |ex| {
                let item = item.clone();
                Box::pin(async move { put_in(ex, item, expected_revision).await })
                    as BoxFuture<'_, Result<MemoryItem, StoreError>>
            })
            .await
            // `StoreError::VersionConflict` 逐个对上 `RepoError::VersionConflict`
            // （kernel 的 `From`），所以结论直接走错误通道，不再需要一个载具枚举。
            .map_err(RepoError::from)
    }

    async fn recall(&self, query: &RecallQuery) -> Result<RecallResult, RepoError> {
        let query = query.clone();
        self.db
            .read(move |ex| {
                let query = query.clone();
                Box::pin(async move { recall_in(ex, &query).await })
                    as BoxFuture<'_, Result<RecallResult, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn confirm(
        &self,
        id: &MemoryId,
        expected_revision: u32,
        at: OffsetDateTime,
    ) -> Result<MemoryItem, RepoError> {
        let id = id.to_string();
        self.db
            .with_write_retry(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    let mut row = require_revision(ex, &id, expected_revision).await?;
                    // 操作者确认绑定具体 id / revision；模型返回的 user_confirmed 字段
                    // 没有写入权限（§9.2、§9.6）——它进不到这条路径上来。
                    row.update()
                        .confirmation("user_confirmed")
                        .updated_at(to_ts(at))
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                    Ok(load_item(ex, &id).await?.expect("刚刚还在"))
                }) as BoxFuture<'_, Result<MemoryItem, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn forget(
        &self,
        id: &MemoryId,
        expected_revision: u32,
        at: OffsetDateTime,
    ) -> Result<MemoryItem, RepoError> {
        let id = id.to_string();
        self.db
            .with_write_retry(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    let mut row = require_revision(ex, &id, expected_revision).await?;
                    row.update()
                        .state(enum_str(&MemoryState::Forgotten))
                        .updated_at(to_ts(at))
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                    // forget 立即停用内容，**并使关键词、向量里的引用失效**（§9.6）。
                    clear_index_in(ex, &id).await?;
                    Ok(load_item(ex, &id).await?.expect("刚刚还在"))
                }) as BoxFuture<'_, Result<MemoryItem, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn put_vector(
        &self,
        id: &MemoryId,
        revision: u32,
        generation: &str,
        vector: Vector,
    ) -> Result<(), RepoError> {
        let id = id.to_string();
        let generation = generation.to_string();
        let now = OffsetDateTime::now_utc();
        self.db
            .with_write_retry(move |ex| {
                let (id, generation, vector) = (id.clone(), generation.clone(), vector.clone());
                Box::pin(async move {
                    // 入库前再确认正文、版本、状态与指纹未变；不满足则**丢弃过期结果**
                    // （§9.5）——丢弃不是错误，所以这里返回 Ok。
                    let Some(row) = MemoryItemRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        return Ok(());
                    };
                    if row.revision != i64::from(revision)
                        || row.state == enum_str(&MemoryState::Forgotten)
                    {
                        tracing::debug!(memory = %id, "丢弃一个过期的向量");
                        return Ok(());
                    }

                    let bytes = encode_vector(&vector.0);
                    let key = vector_id(&id, revision, &row.content_hash, &generation);
                    if let Some(mut existing) = MemoryVectorRow::filter_by_id(&key)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    {
                        existing
                            .update()
                            .dimensions(vector.0.len() as i64)
                            .vector(bytes)
                            .created_at(to_ts(now))
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?;
                        return Ok(());
                    }
                    toasty::create!(MemoryVectorRow {
                        id: key,
                        memory_id: id,
                        revision: i64::from(revision),
                        content_hash: row.content_hash.clone(),
                        generation,
                        dimensions: vector.0.len() as i64,
                        vector: bytes,
                        created_at: to_ts(now),
                    })
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    Ok(())
                }) as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn index_status(&self) -> Result<MemoryIndexStatus, RepoError> {
        self.db
            .read(move |ex| {
                Box::pin(async move {
                    let generations = MemoryIndexGenerationRow::filter(
                        MemoryIndexGenerationRow::fields().active().eq(true),
                    )
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    let active = generations.into_iter().next();

                    let items = MemoryItemRow::all().exec(ex).await.map_err(map_toasty)?;
                    let total = items
                        .iter()
                        .filter(|row| row.state != enum_str(&MemoryState::Forgotten))
                        .count() as u64;

                    let Some(active) = active else {
                        // 没有配置向量模型——这是一个状态，不是一次失败。
                        return Ok(MemoryIndexStatus {
                            space: None,
                            generation: None,
                            state: IndexState::Unconfigured,
                            coverage: 0.0,
                            indexed: 0,
                            total,
                            errors: vec![],
                        });
                    };

                    // **与向量臂用同一个 join**：代次要对，`revision` 与 `content_hash`
                    // 也要对。一条改过正文的记忆留下的旧向量召回不到，那它就不该算进
                    // 覆盖率——两处口径不一致时，"就绪"会是一句假话。
                    let counted = toasty::sql::query(
                        "SELECT COUNT(*) FROM memory_vectors v JOIN memory_items i \
                         ON i.id = v.memory_id AND i.revision = v.revision \
                         AND i.content_hash = v.content_hash \
                         WHERE v.generation = ?1 AND i.state != 'forgotten'",
                    )
                    .bind(active.id.clone())
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    let indexed = counted
                        .first()
                        .and_then(|row| column_i64(row, 0))
                        .unwrap_or(0)
                        .max(0) as u64;

                    Ok(MemoryIndexStatus {
                        space: match &active.space {
                            Some(raw) => Some(decode(raw, "memory_index_generations.space")?),
                            None => None,
                        },
                        generation: Some(active.id.clone()),
                        state: decode(&format!("\"{}\"", active.state), "index state")?,
                        coverage: if total == 0 {
                            0.0
                        } else {
                            indexed as f32 / total as f32
                        },
                        indexed,
                        total,
                        errors: decode(&active.errors, "memory_index_generations.errors")?,
                    })
                }) as BoxFuture<'_, Result<MemoryIndexStatus, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }
}

/// 读一行并核对预期 revision。不符就 [`StoreError::VersionConflict`]，没有就
/// [`StoreError::NotFound`]——两者都在事务里说清楚，出口处由 kernel 的 `From` 搬到
/// [`RepoError`]。
async fn require_revision(
    ex: &mut dyn Executor,
    id: &str,
    expected_revision: u32,
) -> Result<MemoryItemRow, StoreError> {
    let row = MemoryItemRow::filter_by_id(id)
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?
        .ok_or_else(|| StoreError::NotFound {
            what: format!("memory {id}"),
        })?;
    if row.revision != i64::from(expected_revision) {
        return Err(StoreError::VersionConflict {
            expected: expected_revision,
            actual: row.revision.max(0) as u32,
        });
    }
    Ok(row)
}

async fn put_in(
    ex: &mut dyn Executor,
    item: MemoryItem,
    expected_revision: Option<u32>,
) -> Result<MemoryItem, StoreError> {
    let id = item.id.to_string();
    let existing = MemoryItemRow::filter_by_id(&id)
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?;

    if let (Some(expected), Some(row)) = (expected_revision, existing.as_ref())
        && row.revision != i64::from(expected)
    {
        return Err(StoreError::VersionConflict {
            expected,
            actual: row.revision.max(0) as u32,
        });
    }

    let content_hash = ContentHash::of_str(&item.content).as_str().to_string();
    let scope_kind = scope_kind(&item.scope);
    let supersedes = match &item.supersedes {
        Some(reference) => Some(encode(reference)?),
        None => None,
    };
    match existing {
        Some(mut row) => {
            row.update()
                .revision(i64::from(item.revision))
                .content(item.content.clone())
                .content_hash(content_hash.clone())
                .kind(enum_str(&item.kind))
                .scope_kind(scope_kind)
                .scope(encode(&item.scope)?)
                .provenance(enum_str(&item.provenance))
                .confirmation(enum_str(&item.confirmation))
                .state(enum_str(&item.state))
                .observed_at(to_ts(item.observed_at))
                .valid_until(to_ts_opt(item.valid_until))
                .updated_at(to_ts(item.updated_at))
                .extraction(encode(&item.extraction)?)
                .usage_count(i64::try_from(item.usage.count).unwrap_or(i64::MAX))
                .last_used_at(to_ts_opt(item.usage.last_used_at))
                .supersedes(supersedes.clone())
                .exec(ex)
                .await
                .map_err(map_toasty)?;
        }
        None => {
            toasty::create!(MemoryItemRow {
                id: id.clone(),
                revision: i64::from(item.revision),
                content: item.content.clone(),
                content_hash: content_hash.clone(),
                kind: enum_str(&item.kind),
                scope_kind,
                scope: encode(&item.scope)?,
                provenance: enum_str(&item.provenance),
                confirmation: enum_str(&item.confirmation),
                state: enum_str(&item.state),
                observed_at: to_ts(item.observed_at),
                valid_until: to_ts_opt(item.valid_until),
                created_at: to_ts(item.created_at),
                updated_at: to_ts(item.updated_at),
                extraction: encode(&item.extraction)?,
                usage_count: i64::try_from(item.usage.count).unwrap_or(i64::MAX),
                last_used_at: to_ts_opt(item.usage.last_used_at),
                supersedes: supersedes.clone(),
            })
            .exec(ex)
            .await
            .map_err(map_toasty)?;
        }
    }

    // 证据是自己的一张表：**证据与内容版本分别保留**（§9.2）。整批重写，行 ID 由
    // memory + revision + 序号决定，所以同一条证据重复写入是幂等的。
    for (ordinal, evidence) in item.evidence.iter().enumerate() {
        let key = ContentHash::of_str(&format!("{id}/{}/{ordinal}", item.revision))
            .as_str()
            .to_string();
        if MemoryEvidenceRow::filter_by_id(&key)
            .first()
            .exec(ex)
            .await
            .map_err(map_toasty)?
            .is_some()
        {
            continue;
        }
        toasty::create!(MemoryEvidenceRow {
            id: key,
            memory_id: id.clone(),
            revision: i64::from(item.revision),
            ordinal: ordinal as i64,
            reference: encode(&evidence.reference)?,
            provenance: enum_str(&evidence.provenance),
            observed_at: to_ts(evidence.observed_at),
            extracted_from_run: evidence.extracted_from_run.as_ref().map(|r| r.to_string()),
        })
        .exec(ex)
        .await
        .map_err(map_toasty)?;
    }

    // 关键词索引跟着正文走。后台刚入库但尚未生成向量的记忆仍可通过关键词命中（§9.4）。
    let (terms, count) = terms_column(&item.content);
    match MemoryTermsRow::filter_by_id(&id)
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?
    {
        Some(mut row) => {
            row.update()
                .revision(i64::from(item.revision))
                .terms(terms)
                .term_count(count as i64)
                .exec(ex)
                .await
                .map_err(map_toasty)?;
        }
        None => {
            toasty::create!(MemoryTermsRow {
                id: id.clone(),
                memory_id: id.clone(),
                revision: i64::from(item.revision),
                terms,
                term_count: count as i64,
            })
            .exec(ex)
            .await
            .map_err(map_toasty)?;
        }
    }

    Ok(load_item(ex, &id).await?.expect("刚刚写进去的"))
}

async fn clear_index_in(ex: &mut dyn Executor, id: &str) -> Result<(), StoreError> {
    if let Some(row) = MemoryTermsRow::filter_by_id(id)
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?
    {
        row.delete().exec(ex).await.map_err(map_toasty)?;
    }
    let vectors = MemoryVectorRow::filter(MemoryVectorRow::fields().memory_id().eq(id))
        .exec(ex)
        .await
        .map_err(map_toasty)?;
    for row in vectors {
        row.delete().exec(ex).await.map_err(map_toasty)?;
    }
    Ok(())
}

/// 向量臂为什么这一次没跑成。`None` = 跑成了。
fn vector_unavailable(reason: &str) -> Option<String> {
    Some(reason.to_string())
}

async fn recall_in(ex: &mut dyn Executor, query: &RecallQuery) -> Result<RecallResult, StoreError> {
    let candidate_limit = query.candidate_limit.max(1) as usize;
    let wants_keyword = !matches!(query.mode, RetrievalMode::Vector);
    let wants_vector = !matches!(query.mode, RetrievalMode::Keyword);

    let keyword = if wants_keyword {
        keyword_arm(ex, query, candidate_limit).await?
    } else {
        Vec::new()
    };

    let mut vector_note: Option<String> = None;
    let mut coverage = 0.0f32;
    let vector = if wants_vector {
        match vector_arm(ex, query, candidate_limit).await? {
            VectorArm::Ran { ranked, covered } => {
                coverage = covered;
                ranked
            }
            VectorArm::Unavailable { reason } => {
                vector_note = vector_unavailable(&reason);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    // 「按排名融合（RRF），避免直接相加不同量纲的分数」（§9.4）。一条臂没跑时融合退化
    // 成恒等，所以这里不必分支。
    let mut arms: Vec<Vec<String>> = Vec::new();
    if wants_keyword {
        arms.push(keyword);
    }
    if wants_vector && vector_note.is_none() {
        arms.push(vector);
    }
    let ranked = reciprocal_rank_fusion(&arms);

    // 去重后核对当前 revision、来源与冲突状态（§9.4）——`admits` 是"这一次放行哪些状态"
    // 那条规则的唯一实现，自动召回与显式 search 都经它。
    let mut items: Vec<MemoryItem> = Vec::new();
    for id in ranked {
        let Some(item) = load_item(ex, &id).await? else {
            continue;
        };
        if !query.admits(&item) {
            continue;
        }
        if !query.scopes.is_empty() && !query.scopes.contains(&item.scope) {
            continue;
        }
        items.push(item);
        if items.len() >= query.top_k.max(1) as usize {
            break;
        }
    }

    Ok(RecallResult {
        items,
        mode: query.mode,
        degraded: vector_note.is_some(),
        degraded_reason: vector_note,
        vector_coverage: wants_vector.then_some(coverage),
    })
}

/// 关键词臂：`memory_terms` 的 `instr` 命中数按 IDF 加权（§9.4）。返回按分降序的 id。
async fn keyword_arm(
    ex: &mut dyn Executor,
    query: &RecallQuery,
    candidate_limit: usize,
) -> Result<Vec<String>, StoreError> {
    let tokens = lexical_terms(&query.text);
    if tokens.is_empty() {
        // 没有可检索的 token 时不回退成"给你全部"——§9.4：没有足够相关内容时返回空。
        return Ok(Vec::new());
    }

    // IDF 在查询时用 `memory_terms` 行数和每个 token 的命中行数算（§9.4）。
    let total_rows = toasty::sql::query("SELECT COUNT(*) FROM memory_terms")
        .exec(ex)
        .await
        .map_err(map_toasty)?
        .first()
        .and_then(|row| column_i64(row, 0))
        .unwrap_or(0)
        .max(1) as f64;

    let mut scores: std::collections::BTreeMap<String, f64> = Default::default();
    for token in &tokens {
        let needle = format!(" {token} ");
        let rows =
            toasty::sql::query("SELECT memory_id FROM memory_terms WHERE instr(terms, ?1) > 0")
                .bind(needle)
                .exec(ex)
                .await
                .map_err(map_toasty)?;
        let hits = rows.len().max(1) as f64;
        // 平滑的 IDF：常见 token 几乎不加分，罕见 token 加得多。
        let idf = (1.0 + (total_rows + 1.0) / (hits + 1.0)).ln();
        for row in &rows {
            if let Some(id) = crate::db::column_string(row, 0) {
                *scores.entry(id).or_insert(0.0) += idf;
            }
        }
    }

    let mut ranked: Vec<(String, f64)> = scores.into_iter().collect();
    // 分高在前；同分按 id，让结果稳定（同一次查询两遍答案一样）。
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    ranked.truncate(candidate_limit);
    Ok(ranked.into_iter().map(|(id, _)| id).collect())
}

enum VectorArm {
    Ran { ranked: Vec<String>, covered: f32 },
    Unavailable { reason: String },
}

/// 向量臂：当前代次内的**精确**余弦（§9.4「由 Rust 在作用域过滤后做精确余弦检索」）。
///
/// 三道闸都在这一个 SQL 里：代次要对，`revision` 要对，`content_hash` 要对。第二、三道
/// 是"修改记忆时旧 revision 不召回"——一条改过正文的记忆留下的旧向量在这里根本 join 不
/// 上，而不是"排得靠后"。
async fn vector_arm(
    ex: &mut dyn Executor,
    query: &RecallQuery,
    candidate_limit: usize,
) -> Result<VectorArm, StoreError> {
    let Some(probe) = query.query_vector.as_ref() else {
        return Ok(VectorArm::Unavailable {
            reason: "这一次没有查询向量：向量端点不可用或未配置".into(),
        });
    };

    let generations =
        MemoryIndexGenerationRow::filter(MemoryIndexGenerationRow::fields().active().eq(true))
            .exec(ex)
            .await
            .map_err(map_toasty)?;
    let Some(active) = generations.into_iter().next() else {
        return Ok(VectorArm::Unavailable {
            reason: "还没有生效的索引代次：重建尚未追平（§9.5）".into(),
        });
    };
    // 「同维度不代表同一空间」——维度只是第一道，指纹的其余部分由代次 ID 本身承担：
    // MemoryManager 用指纹算代次 ID，所以能走到这里的向量就是这一空间的。
    if active.dimensions != probe.0.len() as i64 {
        return Ok(VectorArm::Unavailable {
            reason: format!(
                "查询向量 {} 维，当前代次 {} 维：不是同一个空间，不比",
                probe.0.len(),
                active.dimensions
            ),
        });
    }

    let rows = toasty::sql::query(
        "SELECT v.memory_id, v.dimensions, v.vector FROM memory_vectors v \
         JOIN memory_items i ON i.id = v.memory_id AND i.revision = v.revision \
         AND i.content_hash = v.content_hash \
         WHERE v.generation = ?1 AND i.state != 'forgotten'",
    )
    .bind(active.id.clone())
    .exec(ex)
    .await
    .map_err(map_toasty)?;

    let indexable = {
        let items = MemoryItemRow::all().exec(ex).await.map_err(map_toasty)?;
        items
            .iter()
            .filter(|row| row.state != enum_str(&MemoryState::Forgotten))
            .count()
    };
    let covered = if indexable == 0 {
        0.0
    } else {
        rows.len() as f32 / indexable as f32
    };

    let mut scored: Vec<(String, f32)> = Vec::with_capacity(rows.len());
    for row in &rows {
        let (Some(id), Some(dimensions), Some(bytes)) = (
            crate::db::column_string(row, 0),
            column_i64(row, 1),
            crate::db::column_bytes(row, 2),
        ) else {
            continue;
        };
        // 截断或结构错误的向量不接受（§9.5）——跳过，不拿半截去比。
        let Some(values) = decode_vector(&bytes, dimensions.max(0) as usize) else {
            tracing::warn!(memory = %id, "向量长度与声明的维度对不上，跳过");
            continue;
        };
        scored.push((id, cosine(&values, &probe.0)));
    }
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    scored.truncate(candidate_limit);

    Ok(VectorArm::Ran {
        ranked: scored.into_iter().map(|(id, _)| id).collect(),
        covered,
    })
}

pub(super) async fn load_item(
    ex: &mut dyn Executor,
    id: &str,
) -> Result<Option<MemoryItem>, StoreError> {
    let Some(row) = MemoryItemRow::filter_by_id(id)
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?
    else {
        return Ok(None);
    };

    let mut evidence_rows =
        MemoryEvidenceRow::filter(MemoryEvidenceRow::fields().memory_id().eq(id))
            .exec(ex)
            .await
            .map_err(map_toasty)?;
    evidence_rows.retain(|e| e.revision == row.revision);
    evidence_rows.sort_by_key(|e| e.ordinal);

    let mut evidence = Vec::with_capacity(evidence_rows.len());
    for e in &evidence_rows {
        evidence.push(Evidence {
            reference: decode(&e.reference, "memory_evidence.reference")?,
            provenance: decode(&format!("\"{}\"", e.provenance), "evidence provenance")?,
            observed_at: from_ts(e.observed_at),
            extracted_from_run: e.extracted_from_run.clone().map(RunId::from_raw),
        });
    }

    Ok(Some(MemoryItem {
        id: MemoryId::from_raw(row.id.clone()),
        revision: row.revision.max(0) as u32,
        content: row.content.clone(),
        kind: decode(&format!("\"{}\"", row.kind), "memory_items.kind")?,
        scope: decode(&row.scope, "memory_items.scope")?,
        provenance: decode(
            &format!("\"{}\"", row.provenance),
            "memory_items.provenance",
        )?,
        confirmation: decode(
            &format!("\"{}\"", row.confirmation),
            "memory_items.confirmation",
        )?,
        state: decode(&format!("\"{}\"", row.state), "memory_items.state")?,
        evidence,
        observed_at: from_ts(row.observed_at),
        valid_until: from_ts_opt(row.valid_until),
        created_at: from_ts(row.created_at),
        updated_at: from_ts(row.updated_at),
        extraction: decode(&row.extraction, "memory_items.extraction")?,
        usage: komo_kernel::types::memory::MemoryUsage {
            count: row.usage_count.max(0) as u64,
            last_used_at: from_ts_opt(row.last_used_at),
        },
        supersedes: match &row.supersedes {
            Some(raw) => Some(decode(raw, "memory_items.supersedes")?),
            None => None,
        },
    }))
}

fn vector_id(memory: &str, revision: u32, content_hash: &str, generation: &str) -> String {
    ContentHash::of_str(&format!("{memory}/{revision}/{content_hash}/{generation}"))
        .as_str()
        .to_string()
}

pub(super) fn scope_kind(scope: &MemoryScope) -> String {
    match scope {
        MemoryScope::Personal => "personal",
        MemoryScope::Project { .. } => "project",
        MemoryScope::Environment { .. } => "environment",
    }
    .to_string()
}

pub(super) fn enum_str<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .expect("这个枚举序列化成一个字符串")
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::types::ids::{EventId, Seq, SessionId};
    use komo_kernel::types::memory::{
        Confirmation, Evidence, EvidenceRef, ExtractionMetadata, MemoryKind, MemoryUsage,
        Provenance,
    };
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-15 08:00:00 UTC);

    async fn temp() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect(dir.path().join("state.db"))
            .await
            .expect("打开库");
        (db, dir)
    }

    fn item(id: &str, content: &str, state: MemoryState) -> MemoryItem {
        MemoryItem {
            id: MemoryId::from_raw(id),
            revision: 1,
            content: content.into(),
            kind: MemoryKind::Preference,
            scope: MemoryScope::Personal,
            provenance: Provenance::UserStatement,
            confirmation: Confirmation::Unconfirmed,
            state,
            evidence: vec![Evidence {
                reference: EvidenceRef::Event {
                    session: SessionId::from_raw("sess-1"),
                    event: EventId::from_raw("evt-1"),
                    seq: Seq(41),
                },
                provenance: Provenance::UserStatement,
                observed_at: NOW,
                extracted_from_run: Some(RunId::from_raw("run-1")),
            }],
            observed_at: NOW,
            valid_until: None,
            created_at: NOW,
            updated_at: NOW,
            extraction: ExtractionMetadata::new("memory-model", None, "v1"),
            usage: MemoryUsage::default(),
            supersedes: None,
        }
    }

    fn query(text: &str, mode: RetrievalMode) -> RecallQuery {
        RecallQuery {
            text: text.into(),
            mode,
            scopes: vec![],
            candidate_limit: 40,
            top_k: 8,
            max_tokens: 1500,
            now: NOW,
            query_vector: None,
            include_states: vec![],
        }
    }

    #[tokio::test]
    async fn an_item_round_trips_with_its_evidence() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        let stored = repo
            .put(item("m-1", "客厅空调设 26 度", MemoryState::Active), None)
            .await
            .unwrap();
        let read = repo.get(&MemoryId::from_raw("m-1")).await.unwrap().unwrap();
        assert_eq!(read, stored);
        assert_eq!(read.evidence.len(), 1);
        assert_eq!(read.observed_at, NOW, "时间不掉精度");
    }

    /// `expected_revision` 不符就 `VersionConflict`。
    #[tokio::test]
    async fn writing_over_an_unexpected_revision_is_refused() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put(item("m-1", "喜欢深色主题", MemoryState::Active), None)
            .await
            .unwrap();

        let mut next = item("m-1", "喜欢浅色主题", MemoryState::Active);
        next.revision = 2;
        let error = repo.put(next.clone(), Some(7)).await.unwrap_err();
        assert_eq!(
            error,
            RepoError::VersionConflict {
                expected: 7,
                actual: 1
            }
        );
        repo.put(next, Some(1)).await.expect("对上了就写得进去");
    }

    /// `confirm` 绑定具体 revision；只有操作者交互走得到这里（§9.2）。
    #[tokio::test]
    async fn confirming_needs_the_expected_revision() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put(item("m-1", "喜欢深色主题", MemoryState::Active), None)
            .await
            .unwrap();

        assert!(matches!(
            repo.confirm(&MemoryId::from_raw("m-1"), 9, NOW).await,
            Err(RepoError::VersionConflict { .. })
        ));
        let confirmed = repo
            .confirm(&MemoryId::from_raw("m-1"), 1, NOW)
            .await
            .unwrap();
        assert_eq!(confirmed.confirmation, Confirmation::UserConfirmed);
    }

    /// `forget` 立即停用内容，**并使关键词、向量里的引用失效**（§9.6）。
    #[tokio::test]
    async fn forgetting_also_invalidates_the_keyword_and_vector_index() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put(item("m-1", "客厅空调设 26 度", MemoryState::Active), None)
            .await
            .unwrap();
        repo.put_vector(
            &MemoryId::from_raw("m-1"),
            1,
            "gen-1",
            Vector(vec![0.1, 0.2]),
        )
        .await
        .unwrap();

        assert!(
            !repo
                .recall(&query("空调", RetrievalMode::Keyword))
                .await
                .unwrap()
                .items
                .is_empty()
        );

        let forgotten = repo
            .forget(&MemoryId::from_raw("m-1"), 1, NOW)
            .await
            .unwrap();
        assert_eq!(forgotten.state, MemoryState::Forgotten);
        assert!(
            repo.recall(&query("空调", RetrievalMode::Keyword))
                .await
                .unwrap()
                .items
                .is_empty(),
            "遗忘之后召回不到"
        );
        // 正文还在——遗忘自动 Memory 不等于删除原文（§9.6）。
        assert!(
            repo.get(&MemoryId::from_raw("m-1"))
                .await
                .unwrap()
                .is_some()
        );
    }

    /// 关键词臂：CJK bigram 让两字查询天然命中，不需要子串后备（§9.4）。
    #[tokio::test]
    async fn the_keyword_arm_matches_a_two_character_chinese_query() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put(item("m-1", "客厅空调设 26 度", MemoryState::Active), None)
            .await
            .unwrap();
        repo.put(
            item("m-2", "用 cargo test 跑测试", MemoryState::Active),
            None,
        )
        .await
        .unwrap();

        let hit = repo
            .recall(&query("空调", RetrievalMode::Keyword))
            .await
            .unwrap();
        assert_eq!(hit.items.len(), 1);
        assert_eq!(hit.items[0].id.as_str(), "m-1");

        let ascii = repo
            .recall(&query("CARGO", RetrievalMode::Keyword))
            .await
            .unwrap();
        assert_eq!(ascii.items.len(), 1, "ASCII 小写化之后命中");
        assert_eq!(ascii.items[0].id.as_str(), "m-2");
    }

    /// 没有足够相关内容时返回空（§9.4）——不是"给你全部"。
    #[tokio::test]
    async fn nothing_relevant_returns_nothing() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put(item("m-1", "客厅空调设 26 度", MemoryState::Active), None)
            .await
            .unwrap();
        assert!(
            repo.recall(&query("量子纠缠", RetrievalMode::Keyword))
                .await
                .unwrap()
                .items
                .is_empty()
        );
        assert!(
            repo.recall(&query("", RetrievalMode::Keyword))
                .await
                .unwrap()
                .items
                .is_empty()
        );
    }

    /// 只有 active 且没过期的才进自动召回（§9.4）。
    #[tokio::test]
    async fn only_recallable_items_come_back() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put(item("m-1", "客厅空调很吵", MemoryState::Candidate), None)
            .await
            .unwrap();
        repo.put(item("m-2", "客厅空调很吵", MemoryState::Contested), None)
            .await
            .unwrap();
        let mut expired = item("m-3", "客厅空调很吵", MemoryState::Active);
        expired.valid_until = Some(NOW - time::Duration::days(1));
        repo.put(expired, None).await.unwrap();

        assert!(
            repo.recall(&query("空调", RetrievalMode::Keyword))
                .await
                .unwrap()
                .items
                .is_empty()
        );
    }

    /// 向量臂跑不成时**如实说降级**，不能把故障解释成"没有相关记忆"（§9.4）。
    ///
    /// 两种模式的收场**不同**，这正是 §9.4 最后一段的两句话：`hybrid` 退化为关键词并
    /// 带着原因（结果照给），`vector` 给不出东西并带着原因——由 MemoryManager 把那一条
    /// 翻成"不可用"，而不是在这里假装成一个空结果。
    #[tokio::test]
    async fn hybrid_and_vector_say_out_loud_that_the_vector_arm_is_unavailable() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put(item("m-1", "客厅空调设 26 度", MemoryState::Active), None)
            .await
            .unwrap();

        for mode in [RetrievalMode::Hybrid, RetrievalMode::Vector] {
            let result = repo.recall(&query("空调", mode)).await.unwrap();
            assert!(result.degraded, "{mode:?} 要标 degraded");
            assert!(result.degraded_reason.is_some(), "{mode:?} 要说原因");
        }
        assert_eq!(
            repo.recall(&query("空调", RetrievalMode::Hybrid))
                .await
                .unwrap()
                .items
                .len(),
            1,
            "hybrid 退化为关键词，结果照给"
        );
        assert!(
            repo.recall(&query("空调", RetrievalMode::Vector))
                .await
                .unwrap()
                .items
                .is_empty(),
            "vector-only 没有向量臂就没有答案——空集 + degraded，由上一层报不可用"
        );

        let keyword = repo
            .recall(&query("空调", RetrievalMode::Keyword))
            .await
            .unwrap();
        assert!(!keyword.degraded, "明确选关键词就不是降级");
    }

    /// 入库前确认版本未变；不满足则**丢弃过期结果**（§9.5）。
    #[tokio::test]
    async fn a_vector_for_a_stale_revision_is_dropped() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put_generation("gen-1", None, IndexState::Building, true)
            .await
            .unwrap();
        repo.put(item("m-1", "客厅空调设 26 度", MemoryState::Active), None)
            .await
            .unwrap();

        repo.put_vector(&MemoryId::from_raw("m-1"), 9, "gen-1", Vector(vec![0.1]))
            .await
            .unwrap();
        assert_eq!(repo.index_status().await.unwrap().indexed, 0, "过期的丢掉");

        repo.put_vector(&MemoryId::from_raw("m-1"), 1, "gen-1", Vector(vec![0.1]))
            .await
            .unwrap();
        assert_eq!(repo.index_status().await.unwrap().indexed, 1);
    }

    /// 没有配置向量模型是一个**状态**，不是一次失败。
    #[tokio::test]
    async fn an_index_with_no_generation_reads_as_unconfigured() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put(item("m-1", "喜欢深色主题", MemoryState::Active), None)
            .await
            .unwrap();
        let status = repo.index_status().await.unwrap();
        assert_eq!(status.state, IndexState::Unconfigured);
        assert_eq!(status.total, 1);
        assert_eq!(status.indexed, 0);
        assert!(status.generation.is_none());
        assert!(repo.active_generation().await.unwrap().is_none());
    }

    // ---------------------------------------------------------------- 向量臂（W6）

    fn space(dimensions: u32) -> komo_kernel::types::model::EmbeddingSpace {
        komo_kernel::types::model::EmbeddingSpace {
            provider: "test".into(),
            endpoint: "memory://test".into(),
            model: "fixed".into(),
            revision: Some("1".into()),
            dimensions,
            preprocessing: "v1".into(),
            document_prefix: String::new(),
            query_prefix: String::new(),
            normalized: true,
            distance: komo_kernel::types::model::DistanceRule::Cosine,
            effort: None,
        }
    }

    fn vector_query(mode: RetrievalMode, probe: &[f32]) -> RecallQuery {
        RecallQuery {
            query_vector: Some(Vector(probe.to_vec())),
            ..query("", mode)
        }
    }

    /// 向量臂：作用域过滤之后做精确余弦，方向最近的排前面（§9.4）。
    #[tokio::test]
    async fn the_vector_arm_ranks_by_cosine_within_the_active_generation() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put_generation("gen-1", Some(&space(2)), IndexState::Ready, true)
            .await
            .unwrap();
        for (id, content, vector) in [
            ("m-1", "客厅空调设 26 度", [1.0, 0.0]),
            ("m-2", "用 cargo test 跑测试", [0.0, 1.0]),
        ] {
            repo.put(item(id, content, MemoryState::Active), None)
                .await
                .unwrap();
            repo.put_vector(&MemoryId::from_raw(id), 1, "gen-1", Vector(vector.to_vec()))
                .await
                .unwrap();
        }

        let hit = repo
            .recall(&vector_query(RetrievalMode::Vector, &[0.9, 0.1]))
            .await
            .unwrap();
        assert!(!hit.degraded, "有查询向量、有生效代次，就不是降级");
        assert_eq!(hit.items.len(), 2);
        assert_eq!(hit.items[0].id.as_str(), "m-1", "方向最近的在前");
        assert_eq!(hit.vector_coverage, Some(1.0));
    }

    /// **修改记忆时旧 revision 不召回**（§14 的验收列第五条）。
    ///
    /// 旧向量在库里还在（下一轮索引才清），但它 join 不上当前的 revision 与正文哈希，所以
    /// 向量臂根本看不见它——这是"排得靠后"与"不存在"的区别。
    #[tokio::test]
    async fn a_vector_for_an_old_revision_is_invisible_to_the_vector_arm() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put_generation("gen-1", Some(&space(2)), IndexState::Ready, true)
            .await
            .unwrap();
        repo.put(item("m-1", "客厅空调设 26 度", MemoryState::Active), None)
            .await
            .unwrap();
        repo.put_vector(
            &MemoryId::from_raw("m-1"),
            1,
            "gen-1",
            Vector(vec![1.0, 0.0]),
        )
        .await
        .unwrap();
        assert_eq!(
            repo.recall(&vector_query(RetrievalMode::Vector, &[1.0, 0.0]))
                .await
                .unwrap()
                .items
                .len(),
            1
        );

        // 改正文 → revision 2。旧向量还在表里，但它说的是上一版的意思。
        let mut next = item("m-1", "客厅空调改成 24 度", MemoryState::Active);
        next.revision = 2;
        repo.put(next, Some(1)).await.unwrap();

        let after = repo
            .recall(&vector_query(RetrievalMode::Vector, &[1.0, 0.0]))
            .await
            .unwrap();
        assert!(after.items.is_empty(), "旧 revision 的向量不能再召回它");
        assert_eq!(after.vector_coverage, Some(0.0), "覆盖率也要如实掉下去");
    }

    /// 同维度但不同代次的向量**不混用**（§9.5「同维度不代表同一空间」）。
    #[tokio::test]
    async fn vectors_from_another_generation_are_never_compared() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        // 生效的是 gen-2，而向量写在 gen-1 上——同样是 2 维。
        repo.put_generation("gen-1", Some(&space(2)), IndexState::Ready, false)
            .await
            .unwrap();
        repo.put_generation("gen-2", Some(&space(2)), IndexState::Ready, true)
            .await
            .unwrap();
        repo.put(item("m-1", "客厅空调设 26 度", MemoryState::Active), None)
            .await
            .unwrap();
        repo.put_vector(
            &MemoryId::from_raw("m-1"),
            1,
            "gen-1",
            Vector(vec![1.0, 0.0]),
        )
        .await
        .unwrap();

        let result = repo
            .recall(&vector_query(RetrievalMode::Vector, &[1.0, 0.0]))
            .await
            .unwrap();
        assert!(result.items.is_empty(), "不是这一代的向量，不拿来比");
        assert_eq!(result.vector_coverage, Some(0.0));
    }

    /// 维度对不上就是**另一个空间**：不比，并且说出来（§9.5）。
    #[tokio::test]
    async fn a_query_vector_of_another_dimension_is_refused_not_compared() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put_generation("gen-1", Some(&space(2)), IndexState::Ready, true)
            .await
            .unwrap();
        repo.put(item("m-1", "客厅空调设 26 度", MemoryState::Active), None)
            .await
            .unwrap();
        repo.put_vector(
            &MemoryId::from_raw("m-1"),
            1,
            "gen-1",
            Vector(vec![1.0, 0.0]),
        )
        .await
        .unwrap();

        let result = repo
            .recall(&vector_query(RetrievalMode::Vector, &[1.0, 0.0, 0.0]))
            .await
            .unwrap();
        assert!(result.degraded);
        assert!(
            result.degraded_reason.as_deref().unwrap().contains("维"),
            "{:?}",
            result.degraded_reason
        );
    }

    /// hybrid：两条臂融合，**两臂都认的那条排在前面**（RRF，§9.4）。
    #[tokio::test]
    async fn hybrid_fuses_both_arms_by_rank() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put_generation("gen-1", Some(&space(2)), IndexState::Ready, true)
            .await
            .unwrap();
        // 两条在关键词臂上同分，所以按 id 排：m-1 第一、m-2 第二。**只有 m-2 有向量**，
        // 于是它在向量臂上是第一——两臂都认的那条应当越过只有一臂认的头名。
        repo.put(item("m-1", "客厅空调很吵", MemoryState::Active), None)
            .await
            .unwrap();
        repo.put(item("m-2", "客厅空调设 26 度", MemoryState::Active), None)
            .await
            .unwrap();
        repo.put_vector(
            &MemoryId::from_raw("m-2"),
            1,
            "gen-1",
            Vector(vec![1.0, 0.0]),
        )
        .await
        .unwrap();

        let keyword_only = repo
            .recall(&query("空调", RetrievalMode::Keyword))
            .await
            .unwrap();
        assert_eq!(
            keyword_only.items[0].id.as_str(),
            "m-1",
            "只看关键词时 m-1 在前"
        );

        let result = repo
            .recall(&RecallQuery {
                query_vector: Some(Vector(vec![1.0, 0.0])),
                ..query("空调", RetrievalMode::Hybrid)
            })
            .await
            .unwrap();
        assert!(!result.degraded);
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].id.as_str(), "m-2", "两臂都认的那条赢");
        assert_eq!(result.vector_coverage, Some(0.5), "覆盖率如实报一半");
    }

    /// 没有查询向量时 hybrid **如实降级**，vector 也标降级（上一层把它翻成"不可用"）。
    #[tokio::test]
    async fn without_a_query_vector_the_hybrid_arm_says_it_degraded() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put(item("m-1", "客厅空调设 26 度", MemoryState::Active), None)
            .await
            .unwrap();

        let hybrid = repo
            .recall(&query("空调", RetrievalMode::Hybrid))
            .await
            .unwrap();
        assert!(hybrid.degraded);
        assert!(hybrid.degraded_reason.is_some());
        assert_eq!(hybrid.items.len(), 1, "关键词臂照常给结果，不是空集");

        let vector = repo
            .recall(&query("空调", RetrievalMode::Vector))
            .await
            .unwrap();
        assert!(vector.degraded, "vector 模式也要标，由上一层报不可用");
    }

    /// 有 embedding、却还没有生效代次（重建中）：hybrid 走关键词并说清原因（§9.5 第 3 步）。
    #[tokio::test]
    async fn a_generation_that_is_still_building_degrades_hybrid_to_keyword() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put_generation("gen-1", Some(&space(2)), IndexState::Building, false)
            .await
            .unwrap();
        repo.put(item("m-1", "客厅空调设 26 度", MemoryState::Active), None)
            .await
            .unwrap();

        let result = repo
            .recall(&RecallQuery {
                query_vector: Some(Vector(vec![1.0, 0.0])),
                ..query("空调", RetrievalMode::Hybrid)
            })
            .await
            .unwrap();
        assert!(result.degraded);
        assert!(
            result.degraded_reason.as_deref().unwrap().contains("代次"),
            "{:?}",
            result.degraded_reason
        );
        assert_eq!(result.items.len(), 1, "关键词维持查询");
    }

    /// `include_states`：contested 只在**明确列出状态**时出（§9.6）。
    #[tokio::test]
    async fn a_contested_memory_only_shows_up_when_asked_for_by_state() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put(
            item("m-1", "客厅空调设 26 度", MemoryState::Contested),
            None,
        )
        .await
        .unwrap();

        assert!(
            repo.recall(&query("空调", RetrievalMode::Keyword))
                .await
                .unwrap()
                .items
                .is_empty(),
            "自动召回够不着"
        );
        let explicit = repo
            .recall(&RecallQuery {
                include_states: vec![MemoryState::Contested],
                ..query("空调", RetrievalMode::Keyword)
            })
            .await
            .unwrap();
        assert_eq!(explicit.items.len(), 1);
    }

    // ---------------------------------------------------------------- 库存与索引目录

    #[tokio::test]
    async fn the_catalog_lists_by_scope_and_state() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put(item("m-1", "喜欢深色主题", MemoryState::Active), None)
            .await
            .unwrap();
        let mut project = item(
            "m-2",
            "这个仓库用 cargo test --workspace",
            MemoryState::Candidate,
        );
        project.scope = MemoryScope::Project {
            project_id: "komo".into(),
        };
        repo.put(project, None).await.unwrap();

        assert_eq!(repo.list(None, None, 100).await.unwrap().len(), 2);
        assert_eq!(
            repo.list(Some(MemoryScope::Personal), None, 100)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            repo.list(None, Some(MemoryState::Candidate), 100)
                .await
                .unwrap()[0]
                .id
                .as_str(),
            "m-2"
        );
    }

    /// 逐字相同的那一条按作用域找回来——「重复来源幂等合并」的第一道闸（§9.6）。
    #[tokio::test]
    async fn the_same_sentence_in_another_scope_is_another_memory() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put(item("m-1", "喜欢深色主题", MemoryState::Active), None)
            .await
            .unwrap();

        assert!(
            repo.find_by_content(&MemoryScope::Personal, "喜欢深色主题")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            repo.find_by_content(
                &MemoryScope::Project {
                    project_id: "komo".into()
                },
                "喜欢深色主题"
            )
            .await
            .unwrap()
            .is_none(),
            "不同作用域的同一句话不是同一条记忆（§9.6）"
        );
    }

    /// 索引候选：还欠向量的那些，按 id 升序，游标之后接着走（§9.5 第 2 步）。
    #[tokio::test]
    async fn pending_vectors_is_a_stable_cursor_over_what_is_still_owed() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        for id in ["m-1", "m-2", "m-3"] {
            repo.put(item(id, &format!("记忆 {id}"), MemoryState::Active), None)
                .await
                .unwrap();
        }
        let first = repo.pending_vectors("gen-1", None, 2).await.unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].memory.as_str(), "m-1");

        let next = repo
            .pending_vectors("gen-1", Some("m-2".into()), 10)
            .await
            .unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].memory.as_str(), "m-3");

        // 写进一条之后它就不欠了。
        repo.put_vector(&MemoryId::from_raw("m-1"), 1, "gen-1", Vector(vec![1.0]))
            .await
            .unwrap();
        let owed = repo.pending_vectors("gen-1", None, 10).await.unwrap();
        assert_eq!(owed.len(), 2);
        assert!(owed.iter().all(|c| c.memory.as_str() != "m-1"));
    }

    /// 切换生效代次：**一个事务里**别人全下线（§9.5 第 4 步），随后清理旧代次的向量。
    #[tokio::test]
    async fn activating_a_generation_retires_every_other_one() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put(item("m-1", "客厅空调设 26 度", MemoryState::Active), None)
            .await
            .unwrap();
        repo.put_generation("gen-1", Some(&space(2)), IndexState::Ready, true)
            .await
            .unwrap();
        repo.put_generation("gen-2", Some(&space(3)), IndexState::Building, false)
            .await
            .unwrap();
        repo.put_vector(
            &MemoryId::from_raw("m-1"),
            1,
            "gen-1",
            Vector(vec![1.0, 0.0]),
        )
        .await
        .unwrap();
        repo.put_vector(
            &MemoryId::from_raw("m-1"),
            1,
            "gen-2",
            Vector(vec![1.0, 0.0, 0.0]),
        )
        .await
        .unwrap();

        repo.activate_generation("gen-2").await.unwrap();
        assert_eq!(
            repo.active_generation().await.unwrap().as_deref(),
            Some("gen-2")
        );

        let removed = repo.prune_generations("gen-2").await.unwrap();
        assert_eq!(removed, 1, "旧代次的向量被清掉");
        assert_eq!(repo.indexed_in("gen-2").await.unwrap(), 1);
        assert_eq!(repo.generations().await.unwrap().len(), 1);
    }

    /// 使用计数**只碰这两列**（§9.2：只度量使用，不增加真实性）。
    #[tokio::test]
    async fn recording_a_use_touches_nothing_but_the_counters() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        let stored = repo
            .put(item("m-1", "喜欢深色主题", MemoryState::Candidate), None)
            .await
            .unwrap();
        repo.record_use(&[MemoryId::from_raw("m-1")], NOW)
            .await
            .unwrap();

        let after = repo.get(&MemoryId::from_raw("m-1")).await.unwrap().unwrap();
        assert_eq!(after.usage.count, 1);
        assert_eq!(after.usage.last_used_at, Some(NOW));
        assert_eq!(after.state, stored.state, "用了十次仍然是候选");
        assert_eq!(after.confirmation, stored.confirmation);
        assert_eq!(after.revision, stored.revision);
    }

    /// 取代链存得下、读得回（§9.6）。
    #[tokio::test]
    async fn a_supersedes_link_round_trips() {
        let (db, _dir) = temp().await;
        let repo = TursoMemoryRepo::new(db);
        repo.put(
            item("m-1", "客厅空调设 26 度", MemoryState::Superseded),
            None,
        )
        .await
        .unwrap();
        let mut newer = item("m-2", "客厅空调改成 24 度", MemoryState::Active);
        newer.supersedes = Some(komo_kernel::types::memory::SupersededRef {
            memory: MemoryId::from_raw("m-1"),
            revision: 1,
        });
        repo.put(newer, None).await.unwrap();

        let read = repo.get(&MemoryId::from_raw("m-2")).await.unwrap().unwrap();
        assert_eq!(
            read.supersedes.unwrap().memory.as_str(),
            "m-1",
            "前向链读得回来"
        );
    }
}
