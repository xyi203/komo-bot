//! 离线整理（§9.8）：对一个作用域里**已有的**记忆去重、取代、标冲突，不新增任何正文。
//!
//! 一次运行的流水线，每一步各有一处不可越过的边：
//!
//! 1. **分组**（纯 Rust，不调模型）：每条用现有检索取近邻，并查集连成组，超出上限按相似度
//!    切开，只有自己一条的组跳过。
//! 2. **游标**：组按成员里最早的 `consolidated_at` 排（`NULL` 最前），成员全都没变的组
//!    不调模型；一次最多处理 `max_groups` 组。
//! 3. **判定**：每组一次记忆模型调用，候选按 §9.9 编号。模型给的是"关系"。
//! 4. **授权**（[`authorize`]）：模型说的关系不等于可以执行的动作。越权的降级成冲突
//!    或整条跳过，报告里写明原因。
//! 5. **落库**：一组一个事务，带预期 revision；判定之后组里有条目变了，整组跳过且不盖游标。
//!
//! 预演与执行各自从头判定，互不依赖：模型不是确定的，预演只回答"现在跑会怎样"。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use komo_kernel::protocol::http::{
    ConsolidateAction, ConsolidateActionReport, ConsolidateGroupReport, ConsolidateGroupStatus,
    ConsolidateMember, ConsolidateOutcome, MemoryConsolidateRequest, MemoryConsolidateResponse,
};
use komo_kernel::traits::{ConsolidationWrite, RepoError};
use komo_kernel::types::ids::MemoryId;
use komo_kernel::types::memory::{
    Confirmation, MemoryItem, MemoryKind, MemoryScope, MemoryState, Provenance, RetrievalMode,
    SupersededRef,
};
use komo_kernel::types::turn::{ReplayMessage, RoundInput, TurnRequest};
use serde::Deserialize;
use time::OffsetDateTime;
use tracing::Instrument;
use tracing::field::Empty;

use super::{MemoryError, MemoryManager};

/// 提示词版本（§9.8）。只进 span，不进库：整理不产生新正文，没有地方可写。
pub const PROMPT_VERSION: &str = "memory-consolidate/v1";

/// 每条取多少个近邻（含它自己）。
const NEIGHBORS: u32 = 6;

const STATES: [MemoryState; 3] = [
    MemoryState::Active,
    MemoryState::Candidate,
    MemoryState::Contested,
];

// ------------------------------------------------------------------ 入口

impl MemoryManager {
    /// `komo memory consolidate`。`apply = false` 一个字都不写。
    pub async fn consolidate(
        &self,
        request: &MemoryConsolidateRequest,
    ) -> Result<MemoryConsolidateResponse, MemoryError> {
        if !self.enabled() {
            return Err(MemoryError::Disabled);
        }
        let config = self.live().consolidate;
        let now = self.clock.now();

        let mut in_range: Vec<MemoryItem> = Vec::new();
        for state in STATES {
            in_range.extend(
                self.catalog
                    .list(request.scope.clone(), Some(state), usize::MAX)
                    .await?,
            );
        }
        in_range.retain(|item| request.kind.is_none_or(|kind| item.kind == kind));

        let mut by_scope: BTreeMap<MemoryScope, Vec<MemoryItem>> = BTreeMap::new();
        for item in in_range {
            by_scope.entry(item.scope.clone()).or_default().push(item);
        }

        let mut groups: Vec<Vec<MemoryItem>> = Vec::new();
        for (_, mut items) in by_scope {
            items.sort_by(|a, b| a.id.cmp(&b.id));
            let neighbors = self.neighbors_of(&items).await;
            for indices in group_items(
                &items,
                &neighbors,
                config.group_size as usize,
                f64::from(config.min_similarity_pct) / 100.0,
            ) {
                groups.push(indices.into_iter().map(|i| items[i].clone()).collect());
            }
        }

        // 游标：最早没看过的组先来；成员都没变的组不调模型。
        let ids: Vec<MemoryId> = groups
            .iter()
            .flatten()
            .map(|item| item.id.clone())
            .collect();
        let marks: HashMap<MemoryId, Option<OffsetDateTime>> = self
            .repo
            .consolidation_marks(&ids)
            .await?
            .into_iter()
            .collect();
        let mark_of = |item: &MemoryItem| marks.get(&item.id).copied().flatten();

        let before = groups.len();
        groups.retain(|group| {
            !group
                .iter()
                .all(|item| mark_of(item).is_some_and(|mark| mark >= item.updated_at))
        });
        let unchanged_groups = (before - groups.len()) as u32;
        groups.sort_by_key(|group| {
            (
                group.iter().map(&mark_of).min().flatten(),
                group[0].id.clone(),
            )
        });
        let cap = config.max_groups as usize;
        let deferred_groups = groups.len().saturating_sub(cap) as u32;
        groups.truncate(cap);

        let mut reports = Vec::with_capacity(groups.len());
        for group in groups {
            reports.push(
                self.consolidate_group(group, request.instruction.as_deref(), request.apply, now)
                    .await,
            );
        }
        Ok(MemoryConsolidateResponse {
            applied: request.apply,
            groups: reports,
            unchanged_groups,
            deferred_groups,
        })
    }

    /// 每条的近邻 id（只含范围内的，不含自己）。检索失败不拦整理：这一条没有近邻而已。
    async fn neighbors_of(&self, items: &[MemoryItem]) -> HashMap<MemoryId, Vec<MemoryId>> {
        let known: HashSet<&MemoryId> = items.iter().map(|item| &item.id).collect();
        let mut out = HashMap::new();
        for item in items {
            let found = self.search_neighbors(item).await;
            let ids: Vec<MemoryId> = found
                .into_iter()
                .map(|found| found.id)
                .filter(|id| *id != item.id && known.contains(id))
                .collect();
            out.insert(item.id.clone(), ids);
        }
        out
    }

    async fn search_neighbors(&self, item: &MemoryItem) -> Vec<MemoryItem> {
        let build = |mode| {
            let mut query = self.query(item.content.clone(), mode);
            query.scopes = vec![item.scope.clone()];
            query.top_k = NEIGHBORS;
            query.include_states = STATES.to_vec();
            query
        };
        match self.search(build(None)).await {
            Ok(result) => result.items,
            Err(MemoryError::VectorUnconfigured) | Err(MemoryError::VectorUnavailable(_)) => self
                .search(build(Some(RetrievalMode::Keyword)))
                .await
                .map(|result| result.items)
                .unwrap_or_default(),
            Err(error) => {
                tracing::warn!(%error, "整理分组时近邻检索失败，这一条按没有近邻处理");
                Vec::new()
            }
        }
    }

    /// 一组：判定 → 授权 → （执行时）落库。每组一个 `komo.memory.consolidate` span，只带
    /// 低基数的数量与结果，不带记忆正文。
    async fn consolidate_group(
        &self,
        group: Vec<MemoryItem>,
        instruction: Option<&str>,
        apply: bool,
        now: OffsetDateTime,
    ) -> ConsolidateGroupReport {
        let span = tracing::info_span!(
            "komo.memory.consolidate",
            komo.memory.group_size = group.len() as u64,
            komo.memory.apply = apply,
            komo.memory.prompt = PROMPT_VERSION,
            komo.memory.actions = Empty,
            komo.memory.merge = Empty,
            komo.memory.supersede = Empty,
            komo.memory.contest = Empty,
            komo.memory.rereadable = Empty,
            komo.memory.outcome = Empty,
        );
        let report = self
            .consolidate_group_inner(&group, instruction, apply, now)
            .instrument(span.clone())
            .await;

        let count = |pick: fn(&ConsolidateAction) -> bool| {
            report
                .actions
                .iter()
                .filter(|entry| pick(&entry.action))
                .count() as u64
        };
        span.record("komo.memory.actions", report.actions.len() as u64);
        span.record(
            "komo.memory.merge",
            count(|a| matches!(a, ConsolidateAction::Merge { .. })),
        );
        span.record(
            "komo.memory.supersede",
            count(|a| matches!(a, ConsolidateAction::Supersede { .. })),
        );
        span.record(
            "komo.memory.contest",
            count(|a| matches!(a, ConsolidateAction::Contest { .. })),
        );
        span.record(
            "komo.memory.rereadable",
            count(|a| matches!(a, ConsolidateAction::Rereadable { .. })),
        );
        span.record(
            "komo.memory.outcome",
            match report.status {
                ConsolidateGroupStatus::Judged if report.actions.is_empty() => "unchanged",
                ConsolidateGroupStatus::Judged => "judged",
                ConsolidateGroupStatus::Conflict => "conflict",
                ConsolidateGroupStatus::Failed => "failed",
            },
        );
        report
    }

    async fn consolidate_group_inner(
        &self,
        group: &[MemoryItem],
        instruction: Option<&str>,
        apply: bool,
        now: OffsetDateTime,
    ) -> ConsolidateGroupReport {
        let mut report = ConsolidateGroupReport {
            scope: group[0].scope.clone(),
            members: group
                .iter()
                .map(|item| ConsolidateMember {
                    memory: item.id.clone(),
                    revision: item.revision,
                })
                .collect(),
            actions: Vec::new(),
            status: ConsolidateGroupStatus::Judged,
            detail: None,
        };

        let raw = match self.judge(group, instruction).await {
            Ok(raw) => raw,
            Err(error) => {
                tracing::warn!(%error, "整理判定没跑成，这一组不动、不盖游标");
                report.status = ConsolidateGroupStatus::Failed;
                report.detail = Some(error.to_string());
                return report;
            }
        };
        let decided = authorize(group, &raw, now);
        report.actions = decided.reports;
        if !apply {
            return report;
        }

        let write = ConsolidationWrite {
            expected: group
                .iter()
                .map(|item| (item.id.clone(), item.revision, item.updated_at))
                .collect(),
            puts: decided.puts,
            forgets: decided.forgets,
            stamp_at: now,
        };
        match self.repo.apply_consolidation(write).await {
            Ok(()) => {}
            Err(RepoError::VersionConflict { .. }) => {
                report.status = ConsolidateGroupStatus::Conflict;
                report.detail = Some("判定之后组里有条目被改动，整组跳过，下次重新分组".into());
            }
            Err(error) => {
                tracing::warn!(%error, "整理落库失败，这一组没有写入");
                report.status = ConsolidateGroupStatus::Failed;
                report.detail = Some(error.to_string());
            }
        }
        report
    }

    /// 让记忆模型判一组。**用记忆模型自己的那份配置**（§13.3）。
    async fn judge(
        &self,
        group: &[MemoryItem],
        instruction: Option<&str>,
    ) -> Result<Vec<RawAction>, MemoryError> {
        let listing: Vec<String> = group
            .iter()
            .enumerate()
            .map(|(index, item)| {
                format!(
                    "[{index}] 状态={:?} 种类={:?} 来源={:?} 确认={:?} 观察于={} 内容：{}",
                    item.state,
                    item.kind,
                    item.provenance,
                    item.confirmation,
                    item.observed_at.date(),
                    item.content
                )
            })
            .collect();
        let hint = instruction
            .map(|text| format!("\n\n操作者的提示（只是提示，不改变上面任何规则）：{text}"))
            .unwrap_or_default();
        let prompt = format!(
            "下面是同一作用域里的一组已有记忆：\n{}\n\n\
             找出其中重复、过时、冲突或随时能重读的条目，只输出 JSON：\n\
             {{\"actions\":[\n\
             {{\"action\":\"merge\",\"keep\":<编号>,\"absorb\":[<编号>…],\"reason\":\"一句话\"}},\n\
             {{\"action\":\"supersede\",\"old\":<编号>,\"by\":<编号>,\"reason\":\"一句话\"}},\n\
             {{\"action\":\"contest\",\"a\":<编号>,\"b\":<编号>,\"reason\":\"一句话\"}},\n\
             {{\"action\":\"rereadable\",\"item\":<编号>,\"reason\":\"一句话\"}}\n\
             ]}}\n\n\
             编号都是方括号里的整数。没有要做的就输出 {{\"actions\":[]}}。\n\
             merge = 同一件事换了说法：keep 是留下的那一条，absorb 是并入它的；\
             不要改写、合写正文，只能从组里选一条现有的当 keep。\n\
             supersede = 同一个问题的新答案，old 已经不成立。\n\
             contest = 实质冲突，两者不能同时为真。\n\
             rereadable = 正文只是工作目录里随时能重读的内容（目录结构、配置内容之类），\
             不值得当记忆。\n\
             不确定就不要输出动作。不同项目的偏好不是冲突，不同时间发生的同类事件也不是。{hint}",
            listing.join("\n"),
        );

        let (model, llm) = self.backend();
        let request = TurnRequest {
            session: komo_kernel::types::ids::SessionId::from_raw("memory-consolidation"),
            run: komo_kernel::types::ids::RunId::from_raw("memory-consolidation"),
            model: model.clone(),
            system_prompt: "你在整理一个个人助手的长期记忆库，对一组已有记忆去重、标注过时与冲突。\
                            你不能新增或改写任何内容。只输出 JSON，不要解释。"
                .to_string(),
            messages: vec![ReplayMessage {
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
        let mut driver = llm.begin_turn(request).await?;
        let round = crate::telemetry::turn(
            &mut *driver,
            RoundInput::First,
            crate::telemetry::role::MEMORY_CONSOLIDATE,
            &model,
            1,
        )
        .await?;
        parse_actions(&round.text.unwrap_or_default(), group.len())
    }
}

// ------------------------------------------------------------------ 分组（纯函数）

/// 把一个作用域里的条目连成组：近邻关系并查集、超限按相似度切开、单条丢掉。
/// 返回的是 `items` 的下标，每组内升序。
fn group_items(
    items: &[MemoryItem],
    neighbors: &HashMap<MemoryId, Vec<MemoryId>>,
    max_size: usize,
    min_similarity: f64,
) -> Vec<Vec<usize>> {
    let position: HashMap<&MemoryId, usize> = items
        .iter()
        .enumerate()
        .map(|(index, item)| (&item.id, index))
        .collect();
    let terms: Vec<HashSet<String>> = items
        .iter()
        .map(|item| {
            komo_store::lexical_terms(&item.content)
                .into_iter()
                .collect()
        })
        .collect();
    let similarity = |a: usize, b: usize| jaccard(&terms[a], &terms[b]);

    let mut parent: Vec<usize> = (0..items.len()).collect();
    fn find(parent: &mut [usize], mut node: usize) -> usize {
        while parent[node] != node {
            parent[node] = parent[parent[node]];
            node = parent[node];
        }
        node
    }
    for (index, item) in items.iter().enumerate() {
        for other in neighbors.get(&item.id).into_iter().flatten() {
            let Some(&other) = position.get(other) else {
                continue;
            };
            if other == index || similarity(index, other) < min_similarity {
                continue;
            }
            let (a, b) = (find(&mut parent, index), find(&mut parent, other));
            if a != b {
                parent[a.max(b)] = a.min(b);
            }
        }
    }

    let mut components: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for index in 0..items.len() {
        let root = find(&mut parent, index);
        components.entry(root).or_default().push(index);
    }
    components
        .into_values()
        .filter(|members| members.len() >= 2)
        .flat_map(|members| split_by_similarity(members, max_size.max(2), &similarity))
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

/// 超过 `max_size` 的连通块：从与其余最相近的一条起，每次并入与当前块最相近的，满了就
/// 另起一块。落单的丢掉。
fn split_by_similarity(
    members: Vec<usize>,
    max_size: usize,
    similarity: &dyn Fn(usize, usize) -> f64,
) -> Vec<Vec<usize>> {
    if members.len() <= max_size {
        return vec![members];
    }
    let mut remaining = members;
    let mut chunks = Vec::new();
    while remaining.len() >= 2 {
        if remaining.len() <= max_size {
            chunks.push(std::mem::take(&mut remaining));
            break;
        }
        let seed = *remaining
            .iter()
            .max_by(|&&a, &&b| {
                let total = |x: usize| -> f64 {
                    remaining
                        .iter()
                        .filter(|&&y| y != x)
                        .map(|&y| similarity(x, y))
                        .sum()
                };
                total(a)
                    .partial_cmp(&total(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(b.cmp(&a))
            })
            .expect("非空");
        remaining.retain(|&x| x != seed);
        let mut chunk = vec![seed];
        while chunk.len() < max_size {
            let next = *remaining
                .iter()
                .max_by(|&&a, &&b| {
                    let best = |x: usize| -> f64 {
                        chunk.iter().map(|&c| similarity(x, c)).fold(0.0, f64::max)
                    };
                    best(a)
                        .partial_cmp(&best(b))
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(b.cmp(&a))
                })
                .expect("非空");
            remaining.retain(|&x| x != next);
            chunk.push(next);
        }
        chunk.sort_unstable();
        chunks.push(chunk);
    }
    chunks
}

// ------------------------------------------------------------------ 解析

/// 模型给的一个动作，编号已校验过范围。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RawAction {
    kind: RawKind,
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RawKind {
    Merge { keep: usize, absorb: Vec<usize> },
    Supersede { old: usize, by: usize },
    Contest { a: usize, b: usize },
    Rereadable { item: usize },
}

#[derive(Debug, Deserialize)]
struct WireActions {
    #[serde(default)]
    actions: Vec<WireAction>,
}

#[derive(Debug, Deserialize)]
struct WireAction {
    action: String,
    #[serde(default)]
    reason: String,
    #[serde(flatten)]
    fields: HashMap<String, serde_json::Value>,
}

/// 编号：整数或数字字符串，且在这次调用的编号表之内（§9.9）。
fn index_of(value: &serde_json::Value, len: usize) -> Option<usize> {
    let index = match value {
        serde_json::Value::Number(number) => number.as_u64(),
        serde_json::Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    }?;
    usize::try_from(index).ok().filter(|&index| index < len)
}

fn parse_actions(text: &str, len: usize) -> Result<Vec<RawAction>, MemoryError> {
    let body = super::extract::parse_json_object(text)
        .ok_or_else(|| MemoryError::Invalid("整理判定里找不到 JSON 对象".into()))?;
    let wire: WireActions = serde_json::from_str(body)
        .map_err(|error| MemoryError::Invalid(format!("整理判定的 JSON 解不开：{error}")))?;

    let mut out = Vec::new();
    for action in wire.actions {
        let field = |name: &str| action.fields.get(name).and_then(|v| index_of(v, len));
        let kind = match action.action.as_str() {
            "merge" => {
                let absorb = match action.fields.get("absorb") {
                    Some(serde_json::Value::Array(values)) => values
                        .iter()
                        .map(|value| index_of(value, len))
                        .collect::<Option<Vec<_>>>(),
                    Some(value) => index_of(value, len).map(|index| vec![index]),
                    None => None,
                };
                field("keep")
                    .zip(absorb)
                    .map(|(keep, absorb)| RawKind::Merge { keep, absorb })
            }
            "supersede" => field("old")
                .zip(field("by"))
                .map(|(old, by)| RawKind::Supersede { old, by }),
            "contest" => field("a")
                .zip(field("b"))
                .map(|(a, b)| RawKind::Contest { a, b }),
            "rereadable" => field("item").map(|item| RawKind::Rereadable { item }),
            _ => None,
        };
        match kind {
            Some(kind) => out.push(RawAction {
                kind,
                reason: action.reason.trim().to_string(),
            }),
            None => {
                tracing::warn!(action = %action.action, "整理判定里的动作编号或类型不可用，丢弃这一条")
            }
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ 授权

/// 授权的结果：报告，加上要写的东西。
struct Decided {
    reports: Vec<ConsolidateActionReport>,
    puts: Vec<MemoryItem>,
    forgets: Vec<MemoryId>,
}

/// 把模型说的关系换成能做的动作（§9.8 的全部授权规则）。**纯函数**：只看组里条目当前
/// 的样子，按顺序在工作副本上应用；后一个动作看见的是前一个动作之后的状态。
fn authorize(group: &[MemoryItem], raw: &[RawAction], now: OffsetDateTime) -> Decided {
    let mut work: Vec<MemoryItem> = group.to_vec();
    let mut consumed = vec![false; group.len()];
    let mut dirty: BTreeSet<usize> = BTreeSet::new();
    let mut forgotten: BTreeSet<usize> = BTreeSet::new();
    let mut reports = Vec::with_capacity(raw.len());
    let id = |index: usize| group[index].id.clone();

    for entry in raw {
        let action = match &entry.kind {
            RawKind::Merge { keep, absorb } => ConsolidateAction::Merge {
                keep: id(*keep),
                absorb: absorb.iter().map(|&i| id(i)).collect(),
            },
            RawKind::Supersede { old, by } => ConsolidateAction::Supersede {
                old: id(*old),
                by: id(*by),
            },
            RawKind::Contest { a, b } => ConsolidateAction::Contest {
                a: id(*a),
                b: id(*b),
            },
            RawKind::Rereadable { item } => ConsolidateAction::Rereadable { item: id(*item) },
        };
        let verdict = match &entry.kind {
            RawKind::Merge { keep, absorb } => {
                merge(&mut work, &mut consumed, &mut dirty, *keep, absorb, now)
            }
            RawKind::Supersede { old, by } => {
                supersede(&mut work, &mut consumed, &mut dirty, *old, *by, now)
            }
            RawKind::Contest { a, b } => {
                if consumed[*a] || consumed[*b] || a == b {
                    Verdict::Skip("条目已被本组前一个动作处理，或两端是同一条".into())
                } else {
                    contest(&mut work, &mut dirty, *a, *b, now);
                    Verdict::Execute
                }
            }
            RawKind::Rereadable { item } => {
                let target = &work[*item];
                if consumed[*item] {
                    Verdict::Skip("条目已被本组前一个动作处理".into())
                } else if target.confirmation != Confirmation::Unconfirmed {
                    Verdict::Skip("已确认的条目不因\u{201c}能重读\u{201d}被遗忘".into())
                } else if target.kind == MemoryKind::Preference {
                    Verdict::Skip("preference 不因\u{201c}能重读\u{201d}被遗忘".into())
                } else {
                    consumed[*item] = true;
                    forgotten.insert(*item);
                    work[*item].state = MemoryState::Forgotten;
                    Verdict::Execute
                }
            }
        };
        let (outcome, executed_as, note) = match verdict {
            Verdict::Execute => (ConsolidateOutcome::Executed, None, None),
            Verdict::Downgrade { to, note } => {
                (ConsolidateOutcome::Downgraded, Some(to), Some(note))
            }
            Verdict::Skip(note) => (ConsolidateOutcome::Skipped, None, Some(note)),
        };
        reports.push(ConsolidateActionReport {
            action,
            reason: entry.reason.clone(),
            outcome,
            executed_as,
            note,
        });
    }

    let puts = dirty
        .iter()
        .filter(|index| !forgotten.contains(index))
        .map(|&index| work[index].clone())
        .collect();
    let forgets = forgotten.iter().map(|&index| id(index)).collect();
    Decided {
        reports,
        puts,
        forgets,
    }
}

enum Verdict {
    Execute,
    /// 降级成冲突。
    Downgrade {
        to: ConsolidateAction,
        note: String,
    },
    Skip(String),
}

fn merge(
    work: &mut [MemoryItem],
    consumed: &mut [bool],
    dirty: &mut BTreeSet<usize>,
    keep: usize,
    absorb: &[usize],
    now: OffsetDateTime,
) -> Verdict {
    let mut absorb: Vec<usize> = absorb.to_vec();
    absorb.sort_unstable();
    absorb.dedup();
    absorb.retain(|&index| index != keep);
    if absorb.is_empty() {
        return Verdict::Skip("没有可吸收的条目".into());
    }
    let involved = || std::iter::once(keep).chain(absorb.iter().copied());
    if involved().any(|index| consumed[index]) {
        return Verdict::Skip("条目已被本组前一个动作处理".into());
    }
    if involved().any(|index| work[index].state == MemoryState::Contested) {
        return Verdict::Skip(
            "合并的成员里有 contested：吸收掉冲突的一方，另一方就没有对手了；\
             contested 只参与 supersede 与 contest。需要人看"
                .into(),
        );
    }
    let keeper = work[keep].clone();
    for &index in &absorb {
        let gone = &work[index];
        if gone.provenance == Provenance::UserStatement
            && keeper.provenance != Provenance::UserStatement
        {
            return Verdict::Skip(
                "keep 的来源弱于被吸收的用户陈述：推断不能吸收用户陈述。需要人看".into(),
            );
        }
        if gone.confirmation == Confirmation::UserConfirmed
            && keeper.confirmation != Confirmation::UserConfirmed
        {
            return Verdict::Skip("被吸收的条目已确认而 keep 没有。需要人看".into());
        }
        if gone.state == MemoryState::Active && keeper.state != MemoryState::Active {
            return Verdict::Skip(
                "被吸收的是 active 而 keep 不是：会绕过授权把一条生效记忆停掉。需要人看".into(),
            );
        }
    }

    // 只补证据；state、confirmation、正文一个字不动。
    let by = SupersededRef {
        memory: keeper.id.clone(),
        revision: keeper.revision,
    };
    let mut grew = false;
    for &index in &absorb {
        let evidence = work[index].evidence.clone();
        for item in evidence {
            if !work[keep]
                .evidence
                .iter()
                .any(|have| have.reference == item.reference)
            {
                work[keep].evidence.push(item);
                grew = true;
            }
        }
        work[index].state = MemoryState::Superseded;
        work[index].superseded_by = Some(by.clone());
        work[index].updated_at = now;
        consumed[index] = true;
        dirty.insert(index);
    }
    if grew {
        work[keep].updated_at = now;
        dirty.insert(keep);
    }
    Verdict::Execute
}

fn supersede(
    work: &mut [MemoryItem],
    consumed: &mut [bool],
    dirty: &mut BTreeSet<usize>,
    old: usize,
    by: usize,
    now: OffsetDateTime,
) -> Verdict {
    if old == by || consumed[old] || consumed[by] {
        return Verdict::Skip("条目已被本组前一个动作处理，或两端是同一条".into());
    }
    let downgrade = if work[by].provenance == Provenance::ModelInference
        && work[old].provenance == Provenance::UserStatement
    {
        Some("模型推断不能取代用户陈述")
    } else if work[old].confirmation == Confirmation::UserConfirmed {
        Some("任何东西都不能取代已确认的条目")
    } else {
        None
    };
    if let Some(why) = downgrade {
        contest(work, dirty, old, by, now);
        let (a, b) = (work[old].id.clone(), work[by].id.clone());
        return Verdict::Downgrade {
            to: ConsolidateAction::Contest { a, b },
            note: format!("{why}：降级为 contest，等人裁"),
        };
    }
    work[old].state = MemoryState::Superseded;
    work[old].superseded_by = Some(SupersededRef {
        memory: work[by].id.clone(),
        revision: work[by].revision,
    });
    work[old].updated_at = now;
    consumed[old] = true;
    dirty.insert(old);
    Verdict::Execute
}

fn contest(
    work: &mut [MemoryItem],
    dirty: &mut BTreeSet<usize>,
    a: usize,
    b: usize,
    now: OffsetDateTime,
) {
    for index in [a, b] {
        if work[index].state != MemoryState::Contested {
            work[index].state = MemoryState::Contested;
            work[index].updated_at = now;
            dirty.insert(index);
        }
    }
}

#[cfg(test)]
mod tests;
