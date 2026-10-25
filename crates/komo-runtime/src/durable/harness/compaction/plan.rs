//! 压不压、换不换：把 kernel 的缓存账（`compaction::decide`）接到 entry 推导出来的上下文上
//! （旧实现是 `komo-agent` 的 `plan` / `plan_overflow`，作用在 `AgentContext` 上）。
//!
//! 决策的时机与旧实现一样只有两种：**缓存已冷**（改写不欠债，有资格的结果全换）、**贴着窗口**
//! （不看账：先换短视图——比压缩便宜，没有可换的再压成摘要）。其余时候什么都不动，短任务从不
//! 改写前缀。pi-durable §8.7 多出一个**后台阈值**：上下文离阻塞阈值还差 `background_tokens`
//! 时，起一个不挡当前轮的后台压缩（默认关）。
//!
//! 一切决定都是纯函数：输入是已提交的 entry、推导出的上下文和钩子给的设置，输出是一个
//! [`Plan`]。生成任务的 `prepare` 调它，溢出恢复调 [`plan_overflow`]。

use komo_kernel::compaction::{
    CompactionDecision, CompactionInput, OnlineState, decide, effective_window,
};
use komo_kernel::durable::compaction::{
    Cut, DecayCandidate, decay_candidates, estimate_messages_tokens, select_cut,
};
use komo_kernel::durable::{ContextView, EntryRecord, Message};
use komo_kernel::projection::DecayPolicy;
use komo_kernel::types::model::ModelConfig;

use super::setup::{CompactionSettings, CompactionSetup};

/// Anthropic Messages 的 `provider` 名：有显式缓存断点，换了中间一条只重写它之后的部分；
/// 其余协议没有断点，改了中间一条就退回系统提示，整段重写（旧实现同）。
const ANTHROPIC_MESSAGES: &str = "anthropic_messages";

/// 一次 `prepare` 的结论。
#[derive(Debug, Clone)]
pub enum Plan {
    /// 不是时候：没贴着窗口（缓存也没冷到有东西可换）；刚压过 / 刚被拒过 / 压不动。
    Nothing,
    /// 把这批有资格的结果换成短视图（一条只带 `edits` 的 write entry）。
    Decay {
        candidates: Vec<DecayCandidate>,
        decision: CompactionDecision,
    },
    /// 贴着窗口：阻塞压缩，生成等它。
    Compact { decision: CompactionDecision },
    /// 到了后台阈值：起一个不挡当前轮的压缩。
    Background { decision: CompactionDecision },
}

pub struct PlanInput<'a> {
    pub entries: &'a [EntryRecord],
    pub view: &'a ContextView,
    /// 这次 `prepare` 计划写的 system 消息：请求会带上它们，估价也要算。
    pub planned: &'a [Message],
    pub online: &'a OnlineState,
    pub setup: &'a CompactionSetup,
    /// 这一轮的模型：窗口与协议从它来。
    pub model: &'a ModelConfig,
    /// 工具结果的衰减策略（`ToolEnv.projection.decay`）；`None` = 不衰减。
    pub decay: Option<DecayPolicy>,
    /// 上一次请求写下的提示缓存已经过期：这次请求无论改不改都要整段重写，改写不欠债。
    pub cache_cold: bool,
    /// `komo.live` 里已经有一个压缩在跑：不再起后台压缩。
    pub compaction_running: bool,
}

/// 估出来的与上一次请求实报的取大（SoL-Pi 同口径）：估计对中日韩文字偏低，实报又不含上一次
/// 回复之后新加的东西。
fn context_tokens(input: &PlanInput<'_>) -> u64 {
    let estimated =
        estimate_messages_tokens(&input.view.messages) + estimate_messages_tokens(input.planned);
    estimated.max(input.online.last_context_tokens.unwrap_or(0))
}

/// 窗口、阻塞阈值：`window - reserve`。
fn threshold(settings: &CompactionSettings, model: &ModelConfig) -> u64 {
    let reserve = settings.economics.window_reserve_tokens;
    effective_window(model.context_window, settings.max_context_tokens, reserve)
        .saturating_sub(reserve)
}

/// 两条路共用的决策输入：在线状态、窗口与设置；改写多少、收掉多少由调用方填。
pub(super) fn base_input(
    online: &OnlineState,
    settings: &CompactionSettings,
    model: &ModelConfig,
    context_tokens: u64,
) -> CompactionInput {
    let reserve = settings.economics.window_reserve_tokens;
    CompactionInput {
        write_tokens: 0,
        archive_tokens: 0,
        memo_tokens: 0,
        context_tokens,
        // 剩余请求按"每个计划步骤花几次请求"估，计划已经没有了：估不出，只剩冷缓存与
        // 窗口保护两条不看账的路。
        completed_boundary_request_counts: None,
        remaining_boundaries: 0,
        average_context_token_increment: online.average_context_token_increment(),
        context_window_tokens: Some(effective_window(
            model.context_window,
            settings.max_context_tokens,
            reserve,
        )),
        prior_compaction_count: online.compaction_count,
        requests_since_last_compaction: online.requests_since_last_compaction(),
        carried_debt_tokens: online.cache_debt_tokens,
        cache_debt_repayment_tokens: online.cache_debt_repayment_tokens,
        cache_write_read_ratio: settings.cache_write_read_ratio,
        cache_cold: false,
    }
}

/// 这一刻压不压 / 换不换。
pub fn plan(input: PlanInput<'_>) -> Plan {
    let settings = &input.setup.settings;
    let tokens = context_tokens(&input);
    let threshold = threshold(settings, input.model);
    let pressure = tokens >= threshold;
    let candidates = input
        .decay
        .as_ref()
        .map(|policy| decay_candidates(input.entries, input.view, policy))
        .unwrap_or_default();
    let decayable = !candidates.is_empty();
    let background_floor = threshold.saturating_sub(settings.background_tokens);
    let background = settings.background_tokens > 0
        && !input.compaction_running
        && input.setup.enabled
        && tokens >= background_floor;
    if !(pressure || (input.cache_cold && decayable) || background) {
        return Plan::Nothing;
    }
    // 两次改写之间至少要有一次真正的请求：刚压过 / 刚换过就再来一次，只会白白重写缓存。
    if input.online.requests_since_last_compaction() == Some(0) {
        return Plan::Nothing;
    }
    let input_for = |write: u64, archive: u64, memo: u64, cold: bool| CompactionInput {
        write_tokens: write,
        archive_tokens: archive,
        memo_tokens: memo,
        cache_cold: cold,
        ..base_input(input.online, settings, input.model, tokens)
    };

    if decayable && (input.cache_cold || pressure) {
        let cold = input.cache_cold;
        let saving: u64 = candidates
            .iter()
            .map(|candidate| {
                candidate
                    .full_tokens
                    .saturating_sub(komo_kernel::compaction::estimate_tokens(&candidate.decayed))
            })
            .sum();
        let write = if input.model.provider == ANTHROPIC_MESSAGES {
            let earliest = candidates
                .iter()
                .map(|candidate| candidate.message_index)
                .min()
                .unwrap_or(0);
            estimate_messages_tokens(
                &input.view.messages[earliest.min(input.view.messages.len())..],
            )
        } else {
            tokens
        };
        let decision = decide(&input_for(write, saving, 0, cold), &settings.economics);
        return Plan::Decay {
            candidates,
            decision,
        };
    }

    // 走到这里：贴着窗口而没有可换的短视图，或只是到了后台阈值。
    if !input.setup.enabled || input.online.compaction_refused {
        return Plan::Nothing;
    }
    let Some(cut) = select_cut(input.view, settings.keep_recent_tokens) else {
        return Plan::Nothing;
    };
    let mut decision = decide(
        &input_for(
            tokens,
            cut.archive_tokens,
            settings.memo_tokens,
            input.cache_cold,
        ),
        &settings.economics,
    );
    if pressure {
        // 压完还贴着窗口：下一次请求又会触发，一轮一次摘要，什么都换不回来。
        if !decision.compact || decision.post_compaction_tokens >= threshold {
            return Plan::Nothing;
        }
        return Plan::Compact { decision };
    }
    // 后台：是个明确打开的阈值，不再过缓存账的闸门；只要摘要真的比被收掉的那段小。
    if cut.archive_tokens <= settings.memo_tokens || decision.post_compaction_tokens >= threshold {
        return Plan::Nothing;
    }
    decision.compact = true;
    Plan::Background { decision }
}

/// provider 已经以上下文超长拒了请求（旧实现的 `plan_overflow`）：不看缓存账、不看 `enabled`
/// （不压这条 run 就失败了），也不判贴不贴窗口（字节 / 4 的估计可能比 provider 的分词低）——按
/// `keep_recent_tokens` 切，强制压一次。
///
/// 不压（`None`）的情形都让 run 照原样失败：上次压成摘要之后一次成功的请求都还没有（刚压过
/// 还是超长，再压只会转圈）；切不出可收的一段（比如只剩最后一轮）；摘要不比那一段小。
pub fn plan_overflow(
    view: &ContextView,
    online: &OnlineState,
    settings: &CompactionSettings,
    model: &ModelConfig,
    cache_cold: bool,
) -> Option<(CompactionDecision, Cut)> {
    if online.requests_since_last_summary() == Some(0) {
        return None;
    }
    let tokens =
        estimate_messages_tokens(&view.messages).max(online.last_context_tokens.unwrap_or(0));
    let cut = select_cut(view, settings.keep_recent_tokens)?;
    let decision = decide(
        &CompactionInput {
            write_tokens: tokens,
            archive_tokens: cut.archive_tokens,
            memo_tokens: settings.memo_tokens,
            cache_cold,
            ..base_input(online, settings, model, tokens)
        },
        &settings.economics,
    )
    .forced_by_overflow();
    decision.compact.then_some((decision, cut))
}
