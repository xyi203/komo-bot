//! 去重、冲突与取代（§9.3 的"去重 / 冲突整理"，§9.6 的全部规则）。
//!
//! 一条观察进库要过三道，顺序不能换：
//!
//! 1. **逐字相同**（同作用域、同正文）→ 合并证据，什么都不新增。「重复来源幂等合并」
//!    （§9.6）；它也是"重复处理相同来源不重复新增"（§9.3）在实现上的兜底——游标漏了
//!    一次，这里还挡得住。
//! 2. **相关条目召回** → 关键词 ∪ 向量，`include_states` 放行候选与 contested，因为
//!    冲突恰恰最可能和一条候选冲突。召回在**提取之前、整批做一次**（[`Consolidator::recall`]），
//!    候选连编号一起放进提取提示词（§9.10）。
//! 3. **关系**由那一次提取调用顺带交回 → same / supports / contradicts / supersedes /
//!    unrelated，编号已在 [`map_verdicts`] 映射回 id。没有单独的判关系调用。
//!
//! 落库时是这个文件真正的内容：**模型说的关系不等于可以执行的动作**。两条不可越过的
//! 线（§9.6）在 [`Plan::authorize`] 里，各有一个测试：
//!
//! - 新推断不能覆盖用户陈述 → `model_inference` 取代 `user_statement`：降级成冲突。
//! - 自动整理不能静默改写用户已确认内容 → 取代一条 `user_confirmed`：降级成冲突。
//!
//! 「使用次数和多次模型复述不能增加确认等级」（§9.6）落在 `Same` 这一支：它只补证据，
//! 一个字都不改 `state` 与 `confirmation`——候选反复被提取出来，仍然是候选。

use komo_kernel::types::ids::MemoryId;
use komo_kernel::types::memory::{
    Confirmation, MemoryItem, MemoryState, MemoryUsage, Provenance, RetrievalMode, SupersededRef,
};
use serde::Deserialize;
use time::OffsetDateTime;

use super::extract::Observation;
use super::{MemoryError, MemoryManager};

/// 一批提取前召回最多放几条已有记忆进提示词（§9.10）。再多，提示词就成了一篇文章。
pub const RECALL_LIMIT: u32 = 20;
/// 召回查询文本（这一批的用户陈述）的字符上限。
const MAX_QUERY_CHARS: usize = 2_000;

/// 新观察与一条已有记忆的关系。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// 同一件事，只是换了说法。
    Same,
    /// 不同的主张，但支持它。
    Supports,
    /// 实质冲突。
    Contradicts,
    /// 取代它（同一个问题，新答案）。
    Supersedes,
    Unrelated,
}

/// 模型对一条已有记忆的判断。`memory` 已经按这次调用的编号表映射回真实 id（§9.9）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub memory: MemoryId,
    pub relation: Relation,
}

/// 模型原样交回的判断：`memory` 是候选编号，是不是整数还没验。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RawVerdict {
    memory: serde_json::Value,
    relation: Relation,
}

/// 判完之后**实际要做**的事。模型说的是 [`Relation`]，能做的是这个。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
    /// 新建一条。
    Insert,
    /// 只往这一条上补证据。状态与确认等级一个字都不动。
    MergeInto(MemoryId),
    /// 双方都标 contested，等用户裁（§9.6）。
    Contest(MemoryId),
    /// 旧的标 superseded，新的建一条并链过去。
    Supersede(MemoryId),
}

impl Plan {
    /// 把模型说的关系换成能做的动作，并在这里挡住两条越权（§9.6）。
    fn authorize(relation: Relation, new: &Observation, existing: &MemoryItem) -> Plan {
        match relation {
            Relation::Same => Plan::MergeInto(existing.id.clone()),
            // 支持一条别的主张：新主张仍然是它自己的一条记忆，旧的那条不动。
            Relation::Supports | Relation::Unrelated => Plan::Insert,
            Relation::Contradicts => Plan::Contest(existing.id.clone()),
            Relation::Supersedes => {
                // **新推断不能覆盖用户陈述**（§9.6）。
                if new.provenance == Provenance::ModelInference
                    && existing.provenance == Provenance::UserStatement
                {
                    tracing::info!(
                        memory = %existing.id,
                        "一条推断想取代用户陈述：降级成冲突，等人裁"
                    );
                    return Plan::Contest(existing.id.clone());
                }
                // **自动整理不能静默改写用户已确认内容**（§9.6）。
                if existing.confirmation == Confirmation::UserConfirmed {
                    tracing::info!(
                        memory = %existing.id,
                        "想取代一条用户已确认的记忆：降级成冲突，等人裁"
                    );
                    return Plan::Contest(existing.id.clone());
                }
                Plan::Supersede(existing.id.clone())
            }
        }
    }
}

/// 把一批观察落进库里。
pub struct Consolidator<'a> {
    manager: &'a MemoryManager,
    /// 这段对话里已经手动记过的条目（`komo memory add`，§9.3）。
    recorded: Vec<MemoryItem>,
}

impl<'a> Consolidator<'a> {
    pub fn new(manager: &'a MemoryManager) -> Self {
        Consolidator {
            manager,
            recorded: Vec::new(),
        }
    }

    /// 告诉它这段对话里哪些已经手动记过：引的用户输入都被其中一条引过的观察，并进那条。
    pub fn with_recorded(mut self, recorded: Vec<MemoryItem>) -> Self {
        self.recorded = recorded;
        self
    }

    /// 这条观察是不是手动记过的那句话又提了一遍：同作用域、同种类，而且它引的每一条证据
    /// 都已经在**唯一一条**手动条目里。只认唯一：两条都盖得住时说不清是哪一条，照常走。
    ///
    /// 代价是用户同一句话里说了两件事、只手动记了一件时，同种类的另一件会被并掉——提示词
    /// 里已经让模型别重提手动记过的那件（`本段已手动记下`），这里是模型没听话时的兜底。
    fn recorded_as(&self, observation: &Observation) -> Option<&MemoryId> {
        if observation.evidence.is_empty() {
            return None;
        }
        let mut covering = self.recorded.iter().filter(|item| {
            item.scope == observation.scope
                && item.kind == observation.kind
                && observation.evidence.iter().all(|evidence| {
                    item.evidence
                        .iter()
                        .any(|have| have.reference == evidence.reference)
                })
        });
        match (covering.next(), covering.next()) {
            (Some(item), None) => Some(&item.id),
            _ => None,
        }
    }

    /// 返回写下去的变更条数。
    pub async fn apply(&self, observations: &[Observation]) -> Result<usize, MemoryError> {
        let mut changes = 0;
        for observation in observations {
            changes += self.apply_one(observation).await?;
        }
        Ok(changes)
    }

    async fn apply_one(&self, observation: &Observation) -> Result<usize, MemoryError> {
        let now = self.manager.clock.now();

        // 第一道：逐字相同的那一条（§9.6 的"重复来源幂等合并"）。
        if let Some(existing) = self
            .manager
            .catalog
            .find_by_content(&observation.scope, &observation.content)
            .await?
        {
            return self.merge(&existing, observation, now).await;
        }

        // 第一道半：这段对话里手动记过的那句话，换个说法又提了一遍（§9.3「手动记一条」）。
        if let Some(id) = self.recorded_as(observation)
            && let Some(existing) = self.manager.repo.get(id).await?
            && matches!(
                existing.state,
                MemoryState::Active | MemoryState::Candidate | MemoryState::Contested
            )
        {
            tracing::debug!(memory = %existing.id, "手动记过的那句话又被提取出来，并进去");
            return self.merge(&existing, observation, now).await;
        }

        let mut changes = 0;
        let mut superseded: Vec<MemoryItem> = Vec::new();
        let mut contested_with: Vec<MemoryId> = Vec::new();

        for verdict in &observation.relations {
            // 重读一遍：同一批里前面的观察可能已经改过它（版本号变了、状态变了）。
            let Some(existing) = self.manager.repo.get(&verdict.memory).await? else {
                continue;
            };
            // 召回跨作用域，但关系只在同作用域内成立；状态也只认召回放行的那几种。
            if existing.scope != observation.scope
                || !matches!(
                    existing.state,
                    MemoryState::Active | MemoryState::Candidate | MemoryState::Contested
                )
            {
                tracing::debug!(memory = %existing.id, "关系指向的条目作用域不同或已退场，忽略");
                continue;
            }
            match Plan::authorize(verdict.relation, observation, &existing) {
                Plan::MergeInto(_) => {
                    changes += self.merge(&existing, observation, now).await?;
                    // 合并掉了就不再新建——它们是同一件事。
                    return Ok(changes);
                }
                Plan::Contest(id) => {
                    changes += self.contest(&existing, now).await?;
                    contested_with.push(id);
                }
                Plan::Supersede(_) => superseded.push(existing),
                Plan::Insert => {}
            }
        }

        let mut observation = observation.clone();
        if !contested_with.is_empty() {
            // 「存在实质冲突时保留证据并标记 contested，暂停正常召回」（§9.6）——
            // **双方都标**，所以新来的这条也进 contested，而不是带着冲突去注入。
            observation.state = MemoryState::Contested;
        }
        // 新 id 先定下来，旧条目才记得下 `superseded_by`。先落新的、再标旧的：中途断了
        // 顶多是新旧并存，不会丢掉这条新事实。
        let new_id = MemoryId::new_at(now);
        let supersedes = superseded.last().map(|old| SupersededRef {
            memory: old.id.clone(),
            revision: old.revision,
        });
        self.insert(new_id.clone(), &observation, supersedes, now)
            .await?;
        changes += 1;
        let by = SupersededRef {
            memory: new_id,
            revision: 1,
        };
        for old in &superseded {
            changes += self.mark_superseded(old, by.clone(), now).await?;
        }
        Ok(changes)
    }

    /// 提取前的相关条目召回（§9.3、§9.10）：用这一批的用户陈述做一次混合检索，**不限
    /// 作用域**（模型写进哪个作用域在提取之后才知道），放行候选与 contested——冲突最可能
    /// 就发生在它们身上。召回不通只返回空：提取照常跑，只是这一批没有候选可关联。
    pub async fn recall(&self, statements: &str) -> Vec<MemoryItem> {
        let text: String = statements.chars().take(MAX_QUERY_CHARS).collect();
        if text.trim().is_empty() {
            return Vec::new();
        }
        let states = vec![
            MemoryState::Active,
            MemoryState::Candidate,
            MemoryState::Contested,
        ];
        let mut query = self.manager.query(text.clone(), None);
        query.top_k = RECALL_LIMIT;
        query.include_states = states.clone();
        match self.manager.search(query).await {
            Ok(result) => result.items,
            Err(MemoryError::VectorUnconfigured) | Err(MemoryError::VectorUnavailable(_)) => {
                // 向量臂不通时仍要整理——退到关键词，而不是把"查不了"当成"没有相关的"。
                let mut fallback = self.manager.query(text, Some(RetrievalMode::Keyword));
                fallback.top_k = RECALL_LIMIT;
                fallback.include_states = states;
                self.manager
                    .search(fallback)
                    .await
                    .map(|result| result.items)
                    .unwrap_or_default()
            }
            Err(error) => {
                tracing::warn!(%error, "相关条目召回失败，这一批没有候选");
                Vec::new()
            }
        }
    }

    /// 合并：**只补证据**。
    ///
    /// 「使用次数和多次模型复述不能增加确认等级」（§9.6）——所以 `state` 与
    /// `confirmation` 原样抄回去，一条候选被提取十次仍然是候选。
    async fn merge(
        &self,
        existing: &MemoryItem,
        observation: &Observation,
        now: OffsetDateTime,
    ) -> Result<usize, MemoryError> {
        let mut merged = existing.clone();
        let before = merged.evidence.len();
        for evidence in &observation.evidence {
            // 同一条证据重复写入是幂等的：按引用去重。
            if merged
                .evidence
                .iter()
                .any(|have| have.reference == evidence.reference)
            {
                continue;
            }
            merged.evidence.push(evidence.clone());
        }
        if merged.evidence.len() == before {
            return Ok(0);
        }
        merged.updated_at = now;
        self.manager
            .repo
            .put(merged, Some(existing.revision))
            .await?;
        Ok(1)
    }

    /// 标 contested：暂停正常召回，**保留证据**，等用户纠正（§9.6）。
    async fn contest(
        &self,
        existing: &MemoryItem,
        now: OffsetDateTime,
    ) -> Result<usize, MemoryError> {
        if existing.state == MemoryState::Contested {
            return Ok(0);
        }
        let mut contested = existing.clone();
        contested.state = MemoryState::Contested;
        contested.updated_at = now;
        self.manager
            .repo
            .put(contested, Some(existing.revision))
            .await?;
        Ok(1)
    }

    /// 旧版本标 superseded 并记下被谁取代（§9.6、§9.8）。正文不动——取代不是改写。
    async fn mark_superseded(
        &self,
        existing: &MemoryItem,
        by: SupersededRef,
        now: OffsetDateTime,
    ) -> Result<usize, MemoryError> {
        let mut old = existing.clone();
        old.state = MemoryState::Superseded;
        old.superseded_by = Some(by);
        old.updated_at = now;
        self.manager.repo.put(old, Some(existing.revision)).await?;
        Ok(1)
    }

    async fn insert(
        &self,
        id: MemoryId,
        observation: &Observation,
        supersedes: Option<SupersededRef>,
        now: OffsetDateTime,
    ) -> Result<MemoryItem, MemoryError> {
        let item = MemoryItem {
            id,
            revision: 1,
            content: observation.content.clone(),
            kind: observation.kind,
            scope: observation.scope.clone(),
            provenance: observation.provenance,
            // **只有操作者的 confirm 抬得起来**（§9.2）。
            confirmation: observation.confirmation(),
            state: observation.state,
            evidence: observation.evidence.clone(),
            observed_at: observation.observed_at,
            valid_until: observation.valid_until,
            created_at: now,
            updated_at: now,
            extraction: observation.extraction.clone(),
            usage: MemoryUsage::default(),
            supersedes,
            superseded_by: None,
        };
        Ok(self.manager.repo.put(item, None).await?)
    }
}

/// 用这次调用的编号表（`candidates` 的下标）把模型交回的关系映射回真实 id（§9.9）。
/// 编号表不落库：越界、非整数的判断丢掉并记 warn；同一编号只认第一条；数字字符串按整数收。
pub fn map_verdicts(raw: Vec<RawVerdict>, candidates: &[MemoryItem]) -> Vec<Verdict> {
    let mut seen = std::collections::HashSet::new();
    let mut verdicts = Vec::new();
    for raw in raw {
        let index = match &raw.memory {
            serde_json::Value::Number(number) => number.as_u64(),
            serde_json::Value::String(text) => text.trim().parse::<u64>().ok(),
            _ => None,
        };
        let Some(item) = index
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| candidates.get(index).map(|item| (index, item)))
        else {
            tracing::warn!(memory = %raw.memory, "关系里的候选编号不可用，丢弃这一条");
            continue;
        };
        if !seen.insert(item.0) {
            tracing::warn!(index = item.0, "关系里同一个编号出现多次，只认第一条");
            continue;
        }
        verdicts.push(Verdict {
            memory: item.1.id.clone(),
            relation: raw.relation,
        });
    }
    verdicts
}
