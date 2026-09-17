//! 去重、冲突与取代（§9.3 的"去重 / 冲突整理"，§9.6 的全部规则）。
//!
//! 一条观察进库要过三道，顺序不能换：
//!
//! 1. **逐字相同**（同作用域、同正文）→ 合并证据，什么都不新增。「重复来源幂等合并」
//!    （§9.6）；它也是"重复处理相同来源不重复新增"（§9.3）在实现上的兜底——游标漏了
//!    一次，这里还挡得住。
//! 2. **相关条目召回** → 关键词 ∪ 向量，`include_states` 放行候选与 contested，因为
//!    冲突恰恰最可能和一条候选冲突。
//! 3. **记忆模型判关系** → same / supports / contradicts / supersedes / unrelated。
//!
//! 判完之后是这个文件真正的内容：**模型说的关系不等于可以执行的动作**。两条不可越过的
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
use komo_kernel::types::turn::{RoundInput, TurnRequest};
use serde::Deserialize;
use time::OffsetDateTime;

use super::extract::Observation;
use super::{MemoryError, MemoryManager};

/// 一次相关条目召回最多看几条。再多，判关系那一问就成了一篇文章。
const RELATED_LIMIT: u32 = 6;

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

/// 模型对一条已有记忆的判断。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Verdict {
    pub memory: String,
    pub relation: Relation,
}

#[derive(Debug, Deserialize)]
struct Verdicts {
    #[serde(default)]
    verdicts: Vec<Verdict>,
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
}

impl<'a> Consolidator<'a> {
    pub fn new(manager: &'a MemoryManager) -> Self {
        Consolidator { manager }
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

        // 第二道：相关条目。**候选与 contested 也要放行**——冲突最可能就发生在它们身上。
        let related = self.related(observation).await;
        if related.is_empty() {
            self.insert(observation, None, now).await?;
            return Ok(1);
        }

        // 第三道：让记忆模型判关系。判不出来（模型报错、JSON 坏了）就当作全部 unrelated：
        // 退回"新建一条候选"，这是本来就安全的那一侧。
        let verdicts = match self.classify(observation, &related).await {
            Ok(verdicts) => verdicts,
            Err(error) => {
                tracing::warn!(%error, "关系判断没跑成，这一条按新建收");
                Vec::new()
            }
        };

        let mut changes = 0;
        let mut supersedes: Option<SupersededRef> = None;
        let mut contested_with: Vec<MemoryId> = Vec::new();

        for verdict in &verdicts {
            let Some(existing) = related
                .iter()
                .find(|item| item.id.as_str() == verdict.memory)
            else {
                // 模型报了一个不在候选集里的 id：它不在这次判断的范围内，忽略。
                continue;
            };
            match Plan::authorize(verdict.relation, observation, existing) {
                Plan::MergeInto(_) => {
                    changes += self.merge(existing, observation, now).await?;
                    // 合并掉了就不再新建——它们是同一件事。
                    return Ok(changes);
                }
                Plan::Contest(id) => {
                    changes += self.contest(existing, now).await?;
                    contested_with.push(id);
                }
                Plan::Supersede(_) => {
                    changes += self.mark_superseded(existing, now).await?;
                    supersedes = Some(SupersededRef {
                        memory: existing.id.clone(),
                        revision: existing.revision,
                    });
                }
                Plan::Insert => {}
            }
        }

        let mut observation = observation.clone();
        if !contested_with.is_empty() {
            // 「存在实质冲突时保留证据并标记 contested，暂停正常召回」（§9.6）——
            // **双方都标**，所以新来的这条也进 contested，而不是带着冲突去注入。
            observation.state = MemoryState::Contested;
        }
        self.insert(&observation, supersedes, now).await?;
        changes += 1;
        Ok(changes)
    }

    /// 相关条目召回（§9.3 的"先关键词 + 向量召回相关条目"）。
    async fn related(&self, observation: &Observation) -> Vec<MemoryItem> {
        let mut query = self.manager.query(observation.content.clone(), None);
        query.scopes = vec![observation.scope.clone()];
        query.top_k = RELATED_LIMIT;
        // 去重与冲突整理要看得见候选与已在争议中的条目。
        query.include_states = vec![
            MemoryState::Active,
            MemoryState::Candidate,
            MemoryState::Contested,
        ];
        match self.manager.search(query).await {
            Ok(result) => result.items,
            Err(MemoryError::VectorUnconfigured) | Err(MemoryError::VectorUnavailable(_)) => {
                // 向量臂不通时仍要整理——退到关键词，而不是把"查不了"当成"没有相关的"。
                let mut fallback = self
                    .manager
                    .query(observation.content.clone(), Some(RetrievalMode::Keyword));
                fallback.scopes = vec![observation.scope.clone()];
                fallback.top_k = RELATED_LIMIT;
                fallback.include_states = vec![
                    MemoryState::Active,
                    MemoryState::Candidate,
                    MemoryState::Contested,
                ];
                self.manager
                    .search(fallback)
                    .await
                    .map(|result| result.items)
                    .unwrap_or_default()
            }
            Err(error) => {
                tracing::warn!(%error, "相关条目召回失败，这一条按新建收");
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

    /// 旧版本标 superseded（§9.6）。正文不动——取代不是改写。
    async fn mark_superseded(
        &self,
        existing: &MemoryItem,
        now: OffsetDateTime,
    ) -> Result<usize, MemoryError> {
        let mut old = existing.clone();
        old.state = MemoryState::Superseded;
        old.updated_at = now;
        self.manager.repo.put(old, Some(existing.revision)).await?;
        Ok(1)
    }

    async fn insert(
        &self,
        observation: &Observation,
        supersedes: Option<SupersededRef>,
        now: OffsetDateTime,
    ) -> Result<MemoryItem, MemoryError> {
        let item = MemoryItem {
            id: MemoryId::new_at(now),
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
        };
        Ok(self.manager.repo.put(item, None).await?)
    }

    /// 让记忆模型判关系。**用记忆模型自己的那份配置**（§13.3）。
    async fn classify(
        &self,
        observation: &Observation,
        related: &[MemoryItem],
    ) -> Result<Vec<Verdict>, MemoryError> {
        let listing: Vec<String> = related
            .iter()
            .map(|item| {
                format!(
                    "- id={} 状态={:?} 来源={:?} 确认={:?} 内容：{}",
                    item.id, item.state, item.provenance, item.confirmation, item.content
                )
            })
            .collect();
        let prompt = format!(
            "新观察（来源 {:?}）：{}\n\n已有的相关记忆：\n{}\n\n\
             对上面每一条，判断新观察与它的关系，只输出 JSON：\n\
             {{\"verdicts\":[{{\"memory\":\"<id>\",\"relation\":\
             \"same|supports|contradicts|supersedes|unrelated\"}}]}}\n\n\
             same = 同一件事换了说法；supports = 不同主张但支持它；\
             contradicts = 实质冲突，两者不能同时为真；\
             supersedes = 同一个问题的新答案，旧的已经不成立；unrelated = 不相干。\n\
             判断冲突前先看作用域和时间：不同项目的偏好不是冲突，\
             不同时间发生的同类事件（比如两次更换滤芯）也不是冲突，各自保留。",
            observation.provenance,
            observation.content,
            listing.join("\n")
        );

        let request = TurnRequest {
            session: komo_kernel::types::ids::SessionId::from_raw("memory-consolidation"),
            run: komo_kernel::types::ids::RunId::from_raw("memory-consolidation"),
            model: self.manager.model.clone(),
            system_prompt: "你在整理一个个人助手的长期记忆库，判断新旧陈述之间的关系。\
                            只输出 JSON，不要解释。"
                .to_string(),
            messages: vec![komo_kernel::types::turn::ReplayMessage {
                role: komo_kernel::types::turn::Role::User,
                seq: komo_kernel::types::ids::Seq::ZERO,
                text: Some(prompt),
                tool_calls: vec![],
                tool_results: vec![],
                provider_blocks: None,
            }],
            tools: vec![],
            memories: vec![],
            covers: None,
        };

        let mut driver = self.manager.llm.begin_turn(request).await?;
        let round = driver.next(RoundInput::First).await?;
        parse_verdicts(&round.text.unwrap_or_default())
    }
}

/// 从模型正文里解出关系判断。和提取那边共用同一套围栏 / 配平括号的处理。
pub fn parse_verdicts(text: &str) -> Result<Vec<Verdict>, MemoryError> {
    let body = super::extract::parse_json_object(text)
        .ok_or_else(|| MemoryError::Invalid("关系判断里找不到 JSON 对象".into()))?;
    let parsed: Verdicts = serde_json::from_str(body)
        .map_err(|error| MemoryError::Invalid(format!("关系判断的 JSON 解不开：{error}")))?;
    Ok(parsed.verdicts)
}
