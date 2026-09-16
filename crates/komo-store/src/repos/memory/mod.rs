//! `MemoryRepo`：MemoryManager 的全部状态读写（§9）。
//!
//! **这是允许写 raw SQL 的三个模块之二**：关键词臂要 `instr`，而 `instr` 不在 toasty
//! 的类型化 API 里。原因往上一层：Turso 的 MVCC 下建不出 FTS 索引（`CREATE INDEX …
//! USING fts` 报 `Custom index modules are not supported in MVCC mode`，§8.2 实测），
//! 所以分词挪到**索引时**——`memory_terms.terms` 是首尾带空格的 token 串，查询时每个
//! token 一个 `instr(terms, ' tok ') > 0`，命中数按 IDF 加权（§9.4）。
//!
//! 这一波只做关键词臂。**向量臂是 W6 的事**，所以 `recall` 在 `hybrid` / `vector` 上
//! 如实把 [`RecallResult::degraded`] 置位并说明原因——§9.4 的"向量服务故障时在检索元信
//! 息中标记 degraded / 原因 / 覆盖率"要的就是这个；把故障解释成"没有相关记忆"是这里
//! 唯一不能说的话。

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

mod terms;

use crate::db::{
    BoxFuture, Db, column_i64, decode, encode, from_ts, from_ts_opt, map_toasty, store_to_repo,
    to_ts, to_ts_opt,
};
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
            .map_err(store_to_repo)
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
            .map_err(store_to_repo)
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
            .map_err(store_to_repo)
    }

    async fn put(
        &self,
        item: MemoryItem,
        expected_revision: Option<u32>,
    ) -> Result<MemoryItem, RepoError> {
        let outcome = self
            .db
            .with_write_retry(move |ex| {
                let item = item.clone();
                Box::pin(async move { put_in(ex, item, expected_revision).await })
                    as BoxFuture<'_, Result<PutOutcome, StoreError>>
            })
            .await
            .map_err(store_to_repo)?;
        outcome.into_result()
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
            .map_err(store_to_repo)
    }

    async fn confirm(
        &self,
        id: &MemoryId,
        expected_revision: u32,
        at: OffsetDateTime,
    ) -> Result<MemoryItem, RepoError> {
        let id = id.to_string();
        let outcome = self
            .db
            .with_write_retry(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    let Some(mut row) = MemoryItemRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        return Ok(PutOutcome::Missing(id));
                    };
                    if row.revision != i64::from(expected_revision) {
                        return Ok(PutOutcome::Conflict {
                            expected: expected_revision,
                            actual: row.revision.max(0) as u32,
                        });
                    }
                    // 操作者确认绑定具体 id / revision；模型返回的 user_confirmed 字段
                    // 没有写入权限（§9.2、§9.6）——它进不到这条路径上来。
                    row.update()
                        .confirmation("user_confirmed")
                        .updated_at(to_ts(at))
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                    let item = load_item(ex, &id).await?.expect("刚刚还在");
                    Ok(PutOutcome::Ok(Box::new(item)))
                }) as BoxFuture<'_, Result<PutOutcome, StoreError>>
            })
            .await
            .map_err(store_to_repo)?;
        outcome.into_result()
    }

    async fn forget(
        &self,
        id: &MemoryId,
        expected_revision: u32,
        at: OffsetDateTime,
    ) -> Result<MemoryItem, RepoError> {
        let id = id.to_string();
        let outcome = self
            .db
            .with_write_retry(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    let Some(mut row) = MemoryItemRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        return Ok(PutOutcome::Missing(id));
                    };
                    if row.revision != i64::from(expected_revision) {
                        return Ok(PutOutcome::Conflict {
                            expected: expected_revision,
                            actual: row.revision.max(0) as u32,
                        });
                    }
                    row.update()
                        .state(enum_str(&MemoryState::Forgotten))
                        .updated_at(to_ts(at))
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                    // forget 立即停用内容，**并使关键词、向量里的引用失效**（§9.6）。
                    clear_index_in(ex, &id).await?;
                    let item = load_item(ex, &id).await?.expect("刚刚还在");
                    Ok(PutOutcome::Ok(Box::new(item)))
                }) as BoxFuture<'_, Result<PutOutcome, StoreError>>
            })
            .await
            .map_err(store_to_repo)?;
        outcome.into_result()
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

                    let bytes: Vec<u8> = vector.0.iter().flat_map(|f| f.to_le_bytes()).collect();
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
            .map_err(store_to_repo)
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

                    let vectors = MemoryVectorRow::filter(
                        MemoryVectorRow::fields()
                            .generation()
                            .eq(active.id.as_str()),
                    )
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    let indexed = vectors.len() as u64;

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
            .map_err(store_to_repo)
    }
}

/// `with_write_retry` 的错误通道只有 [`StoreError`]，而 `put` / `confirm` / `forget`
/// 要答得出 [`RepoError::VersionConflict`]——用一个枚举把结论带出事务，出口处还原。
enum PutOutcome {
    Ok(Box<MemoryItem>),
    Conflict { expected: u32, actual: u32 },
    Missing(String),
}

impl PutOutcome {
    fn into_result(self) -> Result<MemoryItem, RepoError> {
        match self {
            PutOutcome::Ok(item) => Ok(*item),
            PutOutcome::Conflict { expected, actual } => {
                Err(RepoError::VersionConflict { expected, actual })
            }
            PutOutcome::Missing(id) => Err(RepoError::NotFound {
                what: format!("memory {id}"),
            }),
        }
    }
}

async fn put_in(
    ex: &mut dyn Executor,
    item: MemoryItem,
    expected_revision: Option<u32>,
) -> Result<PutOutcome, StoreError> {
    let id = item.id.to_string();
    let existing = MemoryItemRow::filter_by_id(&id)
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?;

    if let (Some(expected), Some(row)) = (expected_revision, existing.as_ref())
        && row.revision != i64::from(expected)
    {
        return Ok(PutOutcome::Conflict {
            expected,
            actual: row.revision.max(0) as u32,
        });
    }

    let content_hash = ContentHash::of_str(&item.content).as_str().to_string();
    let scope_kind = scope_kind(&item.scope);
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

    let stored = load_item(ex, &id).await?.expect("刚刚写进去的");
    Ok(PutOutcome::Ok(Box::new(stored)))
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

async fn recall_in(ex: &mut dyn Executor, query: &RecallQuery) -> Result<RecallResult, StoreError> {
    // TODO(decide: 向量臂是 W6。这一波只有关键词臂，`hybrid` 与 `vector` 都如实标
    // degraded；§9.4 说"未配置向量模型却选择 hybrid / vector 属于配置错误，不能静默变
    // 成长期关键词模式"——那条判断在 MemoryManager 那一层，这里只负责不撒谎。)
    let degraded = !matches!(query.mode, RetrievalMode::Keyword);
    let degraded_reason = degraded.then(|| "向量臂尚未实现（W6），本次只走了关键词".to_string());

    let tokens = lexical_terms(&query.text);
    let candidate_limit = query.candidate_limit.max(1) as usize;

    // 候选：状态可召回 + 没过有效期 + 作用域命中。
    let mut items: Vec<MemoryItem> = Vec::new();
    if tokens.is_empty() {
        // 没有可检索的 token 时不回退成"给你全部"——§9.4：没有足够相关内容时返回空。
        return Ok(RecallResult {
            items,
            mode: query.mode,
            degraded,
            degraded_reason,
            vector_coverage: Some(0.0),
        });
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

    for (id, _) in ranked {
        let Some(item) = load_item(ex, &id).await? else {
            continue;
        };
        if !item.is_recallable_at(query.now) {
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
        degraded,
        degraded_reason,
        vector_coverage: Some(0.0),
    })
}

async fn load_item(ex: &mut dyn Executor, id: &str) -> Result<Option<MemoryItem>, StoreError> {
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
    }))
}

fn vector_id(memory: &str, revision: u32, content_hash: &str, generation: &str) -> String {
    ContentHash::of_str(&format!("{memory}/{revision}/{content_hash}/{generation}"))
        .as_str()
        .to_string()
}

fn scope_kind(scope: &MemoryScope) -> String {
    match scope {
        MemoryScope::Personal => "personal",
        MemoryScope::Project { .. } => "project",
        MemoryScope::Environment { .. } => "environment",
    }
    .to_string()
}

fn enum_str<T: serde::Serialize>(value: &T) -> String {
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

    /// 向量臂还没有——`hybrid` / `vector` 要**如实说降级**，不能把故障解释成"没有相关
    /// 记忆"（§9.4）。
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
            assert!(!result.items.is_empty(), "关键词臂照常给结果");
        }

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
}
