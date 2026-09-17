//! 向量索引与代次（§9.5 的五步）。
//!
//! ```text
//! 1. 空间指纹 → 代次 ID；新代次登记为 building，旧代次保持独立、继续生效
//! 2. 按稳定游标分批 embed；入库前核对正文 / revision / 状态未变，不满足就丢弃
//! 3. 重建期间查询走关键词（hybrid 自动退化，代次还没 active）
//! 4. 追平之后**一个事务**切换 active，随后清理旧代次
//! 5. 重启后接着跑；失败不删记忆正文，也不伪装成索引已完成
//! ```
//!
//! **代次 ID 是空间指纹的函数**，这是整件事的支点：同一个空间重跑多少次都是同一个代次，
//! 所以"重建"天然幂等、崩溃之后天然接得上（游标就是"这一代还欠谁"）；换了模型 / 维度 /
//! 前缀，指纹变，代次 ID 就变，新旧向量放在两个代次里，查询永远不会把新的 query vector
//! 和旧向量比——那正是 §9.5 第 1 步要的。
//!
//! 「模型调用期间不持有数据库事务」（§9.5）：`embed` 在两次仓储调用**之间**发生，
//! [`MemoryCatalog::pending_vectors`] 读一批、放开连接，回来再一条条 `put_vector`。

use std::sync::Arc;

use komo_kernel::protocol::http::IndexState;
use komo_kernel::traits::{EmbeddingClient, MemoryRepo};
use komo_kernel::types::model::{EmbeddingSpace, InputKind};

use super::{MemoryCatalog, MemoryError};

/// 一批多少条。批太大时一次端点故障要丢掉的工作也大；16 是起点。
pub const INDEX_BATCH: usize = 16;

/// 覆盖率达到多少才切换生效代次（§9.5 第 4 步「追平截至切换时的有效条目后事务切换」）。
///
/// **1.0 就是"追平"**：这一代对每一条当前有效的记忆都有一个当前版本的向量。低于 1 的
/// 阈值意味着切换之后仍有条目只靠关键词命中，而那正是"重建中"该有的样子，不是"就绪"。
pub const MIN_ACTIVATION_COVERAGE: f32 = 1.0;

/// 错误列表里最多留几条。它是给人看的，不是日志。
const MAX_ERRORS: usize = 10;

/// 一轮构建的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct IndexOutcome {
    pub generation: String,
    pub indexed: u64,
    pub total: u64,
    pub coverage: f32,
    /// 这一轮结束时这一代是不是生效代次。
    pub activated: bool,
    pub errors: Vec<String>,
}

/// 按当前配置的向量空间把索引建起来。
pub struct IndexBuilder {
    repo: Arc<dyn MemoryRepo>,
    catalog: Arc<dyn MemoryCatalog>,
    embeddings: Arc<dyn EmbeddingClient>,
}

impl std::fmt::Debug for IndexBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexBuilder")
            .field("generation", &Self::generation_id(self.embeddings.space()))
            .finish()
    }
}

impl IndexBuilder {
    pub fn new(
        repo: Arc<dyn MemoryRepo>,
        catalog: Arc<dyn MemoryCatalog>,
        embeddings: Arc<dyn EmbeddingClient>,
    ) -> Self {
        IndexBuilder {
            repo,
            catalog,
            embeddings,
        }
    }

    /// 代次 ID = 空间指纹的前缀。
    ///
    /// 「一个 embedding_space 指纹至少包括：协议与端点身份、模型 ID、版本、实际维度、
    /// 预处理版本、前缀规则、归一化与距离规则；**凭证值不进入指纹**」（§9.5）——那一整
    /// 组字段在 [`EmbeddingSpace`] 里，`fingerprint()` 是它们的规范化哈希，所以这个函数
    /// 不需要知道其中任何一条，加一个字段也不必改它。
    pub fn generation_id(space: &EmbeddingSpace) -> String {
        let fingerprint = space.fingerprint();
        format!(
            "gen-{}",
            &fingerprint.as_str()[..16.min(fingerprint.as_str().len())]
        )
    }

    /// 跑一轮。可以重复调用：已经有向量的条目不会再算一次。
    pub async fn run(&self) -> Result<IndexOutcome, MemoryError> {
        let space = self.embeddings.space().clone();
        let generation = Self::generation_id(&space);

        // 第 1 步：登记代次，**一律不生效**，旧代次（如果有）继续服务，直到第 4 步追平
        // （§9.5 第 1、3 步）。
        //
        // 连**第一代**也不例外，而这不只是对称好看：一个已生效但还空着的代次会让向量臂
        // "跑成了、但一条都没命中"，于是 `vector` 模式返回一个空集而不是"不可用"——正是
        // §9.4 唯一不许说的那句话。没有生效代次时向量臂如实说"重建尚未追平"，hybrid 退
        // 化为关键词，这才是重建期间该有的样子。
        let known = self.catalog.generations().await?;
        if !known.iter().any(|row| row.id == generation) {
            self.catalog
                .put_generation(&generation, Some(&space), IndexState::Building, false)
                .await?;
        } else {
            let indexed = self.catalog.indexed_in(&generation).await?;
            let total = self.catalog.indexable_total().await?;
            self.catalog
                .set_generation_progress(&generation, IndexState::Building, indexed, total, &[])
                .await?;
        }

        // 第 2 步：按稳定游标分批。游标是"上一批最后一个 id"，而**候选集本身**是
        // "还欠向量的那些"——所以崩溃之后重跑不需要把游标存到哪里，重新问一次就是了。
        let mut errors: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let batch = self
                .catalog
                .pending_vectors(&generation, cursor.clone(), INDEX_BATCH)
                .await?;
            if batch.is_empty() {
                break;
            }
            cursor = batch.last().map(|item| item.memory.to_string());

            let texts: Vec<String> = batch.iter().map(|item| item.content.clone()).collect();
            // 文档侧用文档侧的输入规则（§9.5）。这一步在事务外。
            let vectors = match self.embeddings.embed(InputKind::Document, &texts).await {
                Ok(vectors) => vectors,
                Err(error) => {
                    // 「失败不删除记忆正文，也**不伪装成索引已完成**」（§9.5）。
                    errors.push(format!("这一批向量算不出来：{error}"));
                    break;
                }
            };
            if vectors.len() != batch.len() {
                errors.push(format!(
                    "端点返回 {} 条向量，这一批有 {} 条：对不上就不入库",
                    vectors.len(),
                    batch.len()
                ));
                break;
            }

            for (candidate, vector) in batch.iter().zip(vectors) {
                // 「校验返回数量、维度、有限数值和非零范数，不能接受截断或结构错误的
                // 向量」（§9.5）。
                if !vector.is_usable(space.dimensions) {
                    errors.push(format!("{} 的向量不合法，跳过", candidate.memory));
                    continue;
                }
                // `put_vector` 在事务里再核对一次正文 / revision / 状态；不满足则丢弃过期
                // 结果（§9.5）。这里不必抢在它前面判断——那只会多一个会说谎的副本。
                self.repo
                    .put_vector(&candidate.memory, candidate.revision, &generation, vector)
                    .await?;
            }
            errors.truncate(MAX_ERRORS);
        }

        // 第 4 步：追平了就切换，一个事务。
        let indexed = self.catalog.indexed_in(&generation).await?;
        let total = self.catalog.indexable_total().await?;
        let coverage = if total == 0 {
            // 一条可索引的记忆都没有 = 已经追平。空库不是"永远建不完"。
            1.0
        } else {
            indexed as f32 / total as f32
        };

        let already_active = self
            .catalog
            .generations()
            .await?
            .into_iter()
            .any(|row| row.id == generation && row.active);
        let caught_up = errors.is_empty() && coverage >= MIN_ACTIVATION_COVERAGE;
        if caught_up {
            self.catalog.activate_generation(&generation).await?;
            // 「随后按保留规则清理旧索引」（§9.5 第 4 步）。
            let removed = self.catalog.prune_generations(&generation).await?;
            if removed > 0 {
                tracing::info!(removed, keep = %generation, "清理了旧代次的向量");
            }
        } else {
            let state = if errors.is_empty() {
                IndexState::Building
            } else {
                // 「不伪装成索引已完成」——有错就是 failed，下一轮重跑会把它拨回 building。
                IndexState::Failed
            };
            self.catalog
                .set_generation_progress(&generation, state, indexed, total, &errors)
                .await?;
        }

        Ok(IndexOutcome {
            generation,
            indexed,
            total,
            coverage,
            activated: caught_up || already_active,
            errors,
        })
    }
}
