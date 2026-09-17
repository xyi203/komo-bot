//! `MemoryRepo` 之外的那几口：清单、索引候选、代次切换与使用计数。
//!
//! 它们不在 [`komo_kernel::traits::MemoryRepo`] 上，因为那个 trait 说的是"一条记忆的
//! 读写与一次检索"，而这里说的是**库存与索引工程**：谁还没有当前代次的向量、总共有多
//! 少条可索引、哪一代生效。MemoryManager 通过自己那边的 `MemoryCatalog` trait 认它们，
//! 于是 kernel 的接口不必为索引器再长一截。

use komo_kernel::protocol::http::IndexState;
use komo_kernel::traits::{RepoError, StoreError};
use komo_kernel::types::ids::MemoryId;
use komo_kernel::types::memory::{MemoryItem, MemoryScope, MemoryState};
use komo_kernel::types::model::EmbeddingSpace;
use time::OffsetDateTime;

use crate::db::{BoxFuture, column_i64, column_string, decode, encode, map_toasty, to_ts};
use crate::models::{MemoryIndexGenerationRow, MemoryItemRow, MemoryVectorRow};

use super::{TursoMemoryRepo, enum_str, load_item, scope_kind};

/// 一条等着生成向量的记忆（§9.5 的"按稳定游标批量重建"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexCandidate {
    pub memory: MemoryId,
    pub revision: u32,
    pub content: String,
    /// 入库前要核对的正文哈希——模型调用在事务外，回来时正文可能已经变了（§9.5）。
    pub content_hash: String,
}

/// 一个索引代次，读出来的样子。
#[derive(Debug, Clone, PartialEq)]
pub struct GenerationRecord {
    pub id: String,
    pub space: Option<EmbeddingSpace>,
    pub fingerprint: String,
    pub state: IndexState,
    pub dimensions: u32,
    pub active: bool,
    pub created_at: OffsetDateTime,
}

impl TursoMemoryRepo {
    /// 库存清单：按作用域 / 状态筛，按 id（= 创建时间）排序。
    ///
    /// `state = None` 时**不过滤状态**——`komo memory list` 问的是"库里有什么"，而
    /// `forgotten` 也是库里有的东西（正文还在，§9.6）。要"能用的那些"请走 `recall`。
    pub async fn list(
        &self,
        scope: Option<MemoryScope>,
        state: Option<MemoryState>,
        limit: usize,
    ) -> Result<Vec<MemoryItem>, RepoError> {
        let limit = limit.max(1);
        self.db()
            .read(move |ex| {
                let scope = scope.clone();
                Box::pin(async move {
                    let mut rows = MemoryItemRow::all().exec(ex).await.map_err(map_toasty)?;
                    rows.sort_by(|a, b| a.id.cmp(&b.id));
                    let mut out = Vec::new();
                    for row in &rows {
                        if let Some(state) = state
                            && row.state != enum_str(&state)
                        {
                            continue;
                        }
                        if let Some(scope) = &scope {
                            if row.scope_kind != scope_kind(scope) {
                                continue;
                            }
                            let stored: MemoryScope = decode(&row.scope, "memory_items.scope")?;
                            if &stored != scope {
                                continue;
                            }
                        }
                        if let Some(item) = load_item(ex, &row.id).await? {
                            out.push(item);
                        }
                        if out.len() >= limit {
                            break;
                        }
                    }
                    Ok(out)
                }) as BoxFuture<'_, Result<Vec<MemoryItem>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    /// 同一作用域里正文**逐字相同**的那一条（如果有）。
    ///
    /// 「重复来源幂等合并」（§9.6）落到实现上的第一道闸：同一段话被提取第二次时，它先
    /// 撞上这里，于是走的是"补一条证据"而不是"新增一条记忆"。
    pub async fn find_by_content(
        &self,
        scope: &MemoryScope,
        content: &str,
    ) -> Result<Option<MemoryItem>, RepoError> {
        let hash = komo_kernel::types::digest::ContentHash::of_str(content)
            .as_str()
            .to_string();
        let scope = scope.clone();
        self.db()
            .read(move |ex| {
                let (hash, scope) = (hash.clone(), scope.clone());
                Box::pin(async move {
                    let rows = MemoryItemRow::filter(
                        MemoryItemRow::fields().content_hash().eq(hash.as_str()),
                    )
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    for row in &rows {
                        if row.scope_kind != scope_kind(&scope) {
                            continue;
                        }
                        let stored: MemoryScope = decode(&row.scope, "memory_items.scope")?;
                        if stored == scope {
                            return load_item(ex, &row.id).await;
                        }
                    }
                    Ok(None)
                }) as BoxFuture<'_, Result<Option<MemoryItem>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    /// 这一代还欠哪些向量。按 id 升序，`after` 是**稳定游标**：崩溃后从它接着走（§9.5）。
    pub async fn pending_vectors(
        &self,
        generation: &str,
        after: Option<String>,
        limit: usize,
    ) -> Result<Vec<IndexCandidate>, RepoError> {
        let generation = generation.to_string();
        let limit = limit.max(1);
        self.db()
            .read(move |ex| {
                let (generation, after) = (generation.clone(), after.clone());
                Box::pin(async move {
                    // 已经有这一代向量、且正文与版本都还对得上的那些不必再算。
                    let have = toasty::sql::query(
                        "SELECT v.memory_id FROM memory_vectors v JOIN memory_items i \
                         ON i.id = v.memory_id AND i.revision = v.revision \
                         AND i.content_hash = v.content_hash WHERE v.generation = ?1",
                    )
                    .bind(generation)
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    let have: std::collections::BTreeSet<String> = have
                        .iter()
                        .filter_map(|row| column_string(row, 0))
                        .collect();

                    let mut rows = MemoryItemRow::all().exec(ex).await.map_err(map_toasty)?;
                    rows.sort_by(|a, b| a.id.cmp(&b.id));
                    let forgotten = enum_str(&MemoryState::Forgotten);
                    let mut out = Vec::new();
                    for row in rows {
                        if let Some(after) = &after
                            && &row.id <= after
                        {
                            continue;
                        }
                        if row.state == forgotten || have.contains(&row.id) {
                            continue;
                        }
                        out.push(IndexCandidate {
                            memory: MemoryId::from_raw(row.id.clone()),
                            revision: row.revision.max(0) as u32,
                            content: row.content.clone(),
                            content_hash: row.content_hash.clone(),
                        });
                        if out.len() >= limit {
                            break;
                        }
                    }
                    Ok(out)
                }) as BoxFuture<'_, Result<Vec<IndexCandidate>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    /// 可索引的条目总数（遗忘的不算——它的索引引用已经失效了，§9.6）。
    pub async fn indexable_total(&self) -> Result<u64, RepoError> {
        self.db()
            .read(move |ex| {
                Box::pin(async move {
                    let rows = MemoryItemRow::all().exec(ex).await.map_err(map_toasty)?;
                    let forgotten = enum_str(&MemoryState::Forgotten);
                    Ok(rows.iter().filter(|row| row.state != forgotten).count() as u64)
                }) as BoxFuture<'_, Result<u64, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    /// 这一代已经有多少条**当前有效**的向量。
    ///
    /// 「同维度不代表同一空间」（§9.5）的另一半：这里连 `revision` 和 `content_hash` 一起
    /// join，于是一条改过正文的记忆留下的旧向量不算进覆盖率——它本来也召回不到。
    pub async fn indexed_in(&self, generation: &str) -> Result<u64, RepoError> {
        let generation = generation.to_string();
        self.db()
            .read(move |ex| {
                let generation = generation.clone();
                Box::pin(async move {
                    let rows = toasty::sql::query(
                        "SELECT COUNT(*) FROM memory_vectors v JOIN memory_items i \
                         ON i.id = v.memory_id AND i.revision = v.revision \
                         AND i.content_hash = v.content_hash \
                         WHERE v.generation = ?1 AND i.state != 'forgotten'",
                    )
                    .bind(generation)
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                    Ok(rows
                        .first()
                        .and_then(|row| column_i64(row, 0))
                        .unwrap_or(0)
                        .max(0) as u64)
                }) as BoxFuture<'_, Result<u64, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    /// 全部代次，新的在前。
    pub async fn generations(&self) -> Result<Vec<GenerationRecord>, RepoError> {
        self.db()
            .read(move |ex| {
                Box::pin(async move {
                    let mut rows = MemoryIndexGenerationRow::all()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                    rows.sort_by_key(|row| std::cmp::Reverse(row.created_at));
                    let mut out = Vec::with_capacity(rows.len());
                    for row in &rows {
                        out.push(GenerationRecord {
                            id: row.id.clone(),
                            space: match &row.space {
                                Some(raw) => Some(decode(raw, "memory_index_generations.space")?),
                                None => None,
                            },
                            fingerprint: row.fingerprint.clone(),
                            state: decode(&format!("\"{}\"", row.state), "index state")?,
                            dimensions: row.dimensions.max(0) as u32,
                            active: row.active,
                            created_at: crate::db::from_ts(row.created_at),
                        });
                    }
                    Ok(out)
                }) as BoxFuture<'_, Result<Vec<GenerationRecord>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    /// 记下这一代的进度与错误。**不切换生效代次**——那是 [`Self::activate_generation`]。
    pub async fn set_generation_progress(
        &self,
        generation: &str,
        state: IndexState,
        indexed: u64,
        total: u64,
        errors: &[String],
    ) -> Result<(), RepoError> {
        let generation = generation.to_string();
        let errors = errors.to_vec();
        self.db()
            .with_write_retry(move |ex| {
                let (generation, errors) = (generation.clone(), errors.clone());
                Box::pin(async move {
                    let Some(mut row) = MemoryIndexGenerationRow::filter_by_id(&generation)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    else {
                        return Ok(());
                    };
                    row.update()
                        .state(enum_str(&state))
                        .indexed(i64::try_from(indexed).unwrap_or(i64::MAX))
                        .total(i64::try_from(total).unwrap_or(i64::MAX))
                        .errors(encode(&errors)?)
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                    Ok(())
                }) as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    /// 切换生效代次：**一个事务里**别的代次全部下线、这一代上线并标 ready（§9.5 第 4 步）。
    ///
    /// 分成两次写就有一个"两代都生效"或"一代都不生效"的窗口，而查询在那个窗口里会把新的
    /// query vector 和旧代次的向量放在一起比——那正是代次这件事要防的。
    pub async fn activate_generation(&self, generation: &str) -> Result<(), RepoError> {
        let generation = generation.to_string();
        let now = OffsetDateTime::now_utc();
        self.db()
            .with_write_retry(move |ex| {
                let generation = generation.clone();
                Box::pin(async move {
                    let rows = MemoryIndexGenerationRow::all()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                    for mut row in rows {
                        let mine = row.id == generation;
                        if !mine && !row.active {
                            continue;
                        }
                        let update = row.update();
                        let update = if mine {
                            update
                                .active(true)
                                .state(enum_str(&IndexState::Ready))
                                .activated_at(to_ts(now))
                        } else {
                            update.active(false)
                        };
                        update.exec(ex).await.map_err(map_toasty)?;
                    }
                    Ok(())
                }) as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    /// 清理旧代次：除 `keep` 之外的代次连同它们的向量一起删（§9.5 第 4 步的后半）。
    ///
    /// **只删索引，不删记忆正文**：失败的重建不删记忆正文，也不伪装成索引已完成（§9.5）。
    pub async fn prune_generations(&self, keep: &str) -> Result<u64, RepoError> {
        let keep = keep.to_string();
        self.db()
            .with_write_retry(move |ex| {
                let keep = keep.clone();
                Box::pin(async move {
                    let rows = MemoryIndexGenerationRow::all()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                    let mut removed: u64 = 0;
                    for row in rows {
                        if row.id == keep {
                            continue;
                        }
                        let vectors = MemoryVectorRow::filter(
                            MemoryVectorRow::fields().generation().eq(row.id.as_str()),
                        )
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                        for vector in vectors {
                            vector.delete().exec(ex).await.map_err(map_toasty)?;
                            removed += 1;
                        }
                        row.delete().exec(ex).await.map_err(map_toasty)?;
                    }
                    Ok(removed)
                }) as BoxFuture<'_, Result<u64, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    /// 记一次使用。**只度量使用，不增加真实性**（§9.2）——所以它碰的只有这两列。
    pub async fn record_use(&self, ids: &[MemoryId], at: OffsetDateTime) -> Result<(), RepoError> {
        if ids.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        self.db()
            .with_write_retry(move |ex| {
                let ids = ids.clone();
                Box::pin(async move {
                    for id in &ids {
                        let Some(mut row) = MemoryItemRow::filter_by_id(id)
                            .first()
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?
                        else {
                            continue;
                        };
                        let count = row.usage_count.saturating_add(1);
                        row.update()
                            .usage_count(count)
                            .last_used_at(to_ts(at))
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?;
                    }
                    Ok(())
                }) as BoxFuture<'_, Result<(), StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }
}

/// 让上面的方法拿得到 `Db`，同时不把这个字段公开出去。
impl TursoMemoryRepo {
    pub(super) fn db(&self) -> &crate::db::Db {
        &self.db
    }
}
