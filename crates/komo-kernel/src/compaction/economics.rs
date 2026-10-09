//! 压缩划不划算：在一个计划边界上，用缓存账算"现在把早先的上下文换成摘要"值不值。
//!
//! 压缩的代价是一次性的：摘要之后的整段提示都要重新写进缓存（缓存写比缓存读贵）。收益
//! 是往后每一次请求都少读 `archive - memo` 个 token。所以要估的是**往后还有几次请求**：
//! 按已完成的步骤平均花了几次请求、计划里还剩几步来估，再用上下文窗口卡一个上界。
//! 回本所需的请求数不超过这个估计才压。窗口快满了则不看账，直接压。
//!
//! 公式逐条照搬 SoL-Pi 的 `economics.ts`，数也按它的口径：token 数是整数，比率与
//! 回本请求数是浮点。

use serde::{Deserialize, Serialize};

/// 决策的几个旋钮。默认值与 SoL-Pi 相同。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CompactionEconomics {
    /// 剩余请求估计的整体缩放。
    pub remaining_request_scale: f64,
    /// 每步请求数取 `均值 - k × 标准差` 作保守下界；0 = 直接用均值。
    pub remaining_request_stddev_k: f64,
    /// 离窗口还剩这么多 token 时不看账，直接压。
    pub window_reserve_tokens: u64,
    /// 第一次压缩时把剩余请求估计放大的倍数（仍不超过窗口上界）。
    pub first_compaction_request_scale: f64,
    /// 之后的压缩要求 `回本请求数 × margin ≤ 剩余请求`。
    pub subsequent_compaction_margin: f64,
    /// 上次压缩之后至少过了这么多次请求，才按经济账再压。
    pub minimum_requests_since_compaction: u64,
}

impl Default for CompactionEconomics {
    fn default() -> Self {
        Self {
            remaining_request_scale: 1.0,
            remaining_request_stddev_k: 0.0,
            window_reserve_tokens: 16_384,
            first_compaction_request_scale: 2.0,
            subsequent_compaction_margin: 1.5,
            minimum_requests_since_compaction: 2,
        }
    }
}

/// 压或不压的理由。它会落进事件，所以是线格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    /// 账算得过来。
    Economic,
    /// 离窗口太近：不看账。
    WindowProtection,
    /// 账算不过来。
    DeferredEconomic,
    /// 不是第一次：回本请求数乘上 margin 之后超过了剩余请求。
    DeferredSubsequentMargin,
    /// 不是第一次：连同前几次没还清的缓存债一起算，回本不了。
    DeferredCarriedDebt,
    /// 账算得过来，但上次压缩才过去没几次请求。
    DeferredPostCompactionCooldown,
    /// 估不出剩余请求。
    HorizonUnavailable,
    /// 不知道缓存写读价格比。
    CacheRatioUnavailable,
    /// 算得过来，但切不出可以收成摘要的那一段。
    NativeNotCompactable,
    /// 摘要不比原文短。
    NonPositiveSaving,
}

/// [`estimate_remaining_requests`] 的输入。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HorizonInput<'a> {
    /// 已完成的每一步各花了几次请求。
    pub completed_boundary_request_counts: &'a [u64],
    /// 计划里还没完成的步数。
    pub remaining_boundaries: u64,
    pub scale: f64,
    pub standard_deviation_k: f64,
    pub context_tokens: u64,
    pub context_window_tokens: Option<u64>,
    /// 每次请求上下文平均涨多少 token（只算涨的那些次）。
    pub average_context_token_increment: Option<f64>,
}

/// 往后还有几次请求的估计。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestHorizon {
    pub completed_boundary_request_counts: Vec<u64>,
    pub requests_per_boundary_mean: f64,
    pub requests_per_boundary_lower_bound: f64,
    /// 不看窗口时的估计：`1 + ⌊下界 × 剩余步数 × scale⌋`。
    pub unbounded_expected_remaining_requests: u64,
    pub average_context_token_increment: Option<f64>,
    /// 按平均涨幅，窗口还能装下几次请求。
    pub window_request_upper_bound: Option<u64>,
    /// 两者取小。
    pub expected_remaining_requests: u64,
}

/// 少于这么多个样本时不算方差，直接把均值打对折。
const MINIMUM_VARIANCE_SAMPLES: usize = 3;
const SMALL_SAMPLE_SCALE: f64 = 0.5;

pub fn estimate_remaining_requests(input: &HorizonInput<'_>) -> RequestHorizon {
    let counts = input.completed_boundary_request_counts;
    let mean = counts.iter().sum::<u64>() as f64 / counts.len().max(1) as f64;
    let mut lower_bound = mean;
    if input.standard_deviation_k != 0.0 {
        if counts.len() < MINIMUM_VARIANCE_SAMPLES {
            lower_bound *= SMALL_SAMPLE_SCALE;
        } else {
            let variance: f64 = counts.iter().map(|&c| (c as f64 - mean).powi(2)).sum();
            let deviation = (variance / (counts.len() - 1) as f64).sqrt();
            lower_bound = (mean - input.standard_deviation_k * deviation).max(0.0);
        }
    }

    let unbounded = 1
        + (lower_bound * input.remaining_boundaries as f64 * input.scale)
            .floor()
            .max(0.0) as u64;
    let window_upper = match (
        input.context_window_tokens,
        input.average_context_token_increment,
    ) {
        (Some(window), Some(increment)) if increment > 0.0 => Some(
            ((window as f64 - input.context_tokens as f64) / increment)
                .floor()
                .max(0.0) as u64,
        ),
        _ => None,
    };

    RequestHorizon {
        completed_boundary_request_counts: counts.to_vec(),
        requests_per_boundary_mean: mean,
        requests_per_boundary_lower_bound: lower_bound,
        unbounded_expected_remaining_requests: unbounded,
        average_context_token_increment: input.average_context_token_increment,
        window_request_upper_bound: window_upper,
        expected_remaining_requests: window_upper.map_or(unbounded, |upper| unbounded.min(upper)),
    }
}

/// [`decide`] 的输入。
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionInput {
    /// 这次请求整段提示的 token 数（压缩后要重新写进缓存的就是它的剩余部分）。
    pub write_tokens: u64,
    /// 切点之前、要收成摘要的那一段。
    pub archive_tokens: u64,
    /// 摘要本身的估计。
    pub memo_tokens: u64,
    pub context_tokens: u64,
    /// `None` = 估不出剩余请求。
    pub completed_boundary_request_counts: Option<Vec<u64>>,
    pub remaining_boundaries: u64,
    pub average_context_token_increment: Option<f64>,
    pub context_window_tokens: Option<u64>,
    pub prior_compaction_count: u64,
    /// 上次压缩之后过了几次请求；从没压过是 `None`。
    pub requests_since_last_compaction: Option<u64>,
    /// 前几次压缩欠下、还没还清的缓存债。
    pub carried_debt_tokens: f64,
    /// 前几次压缩每次请求还的债。
    pub cache_debt_repayment_tokens: u64,
    /// 缓存写与缓存读的价格比；不知道是 `None`。
    pub cache_write_read_ratio: Option<f64>,
}

/// 一次决策的全部算式中间量与结论。它会落进事件，事后能逐项核对。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionDecision {
    pub write_tokens: u64,
    pub archive_tokens: u64,
    pub memo_tokens: u64,
    /// 压缩之后（含摘要）要重新写进缓存的上下文。
    pub post_compaction_tokens: u64,
    pub context_tokens: u64,
    pub horizon: Option<RequestHorizon>,
    pub breakeven_requests: Option<f64>,
    pub combined_breakeven_requests: Option<f64>,
    pub effective_horizon_requests: Option<f64>,
    pub cache_write_read_ratio: Option<f64>,
    pub incremental_cache_cost_ratio: Option<f64>,
    pub prior_compaction_count: u64,
    pub requests_since_last_compaction: Option<u64>,
    pub carried_debt_tokens: f64,
    pub cache_debt_repayment_tokens: u64,
    pub compact: bool,
    pub reason: CompactionReason,
}

/// 一次压缩欠下的缓存债，和它之后每次请求还多少。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CompactionDebt {
    pub debt_tokens: f64,
    pub repayment_tokens: u64,
}

impl CompactionDecision {
    /// 切不出可以收成摘要的那一段时，把"压"改成不压。不压的决策原样返回。
    pub fn mark_not_compactable(mut self) -> Self {
        if self.compact {
            self.compact = false;
            self.reason = CompactionReason::NativeNotCompactable;
        }
        self
    }

    /// 照这个决策压了之后欠下的债。
    pub fn debt(&self) -> CompactionDebt {
        CompactionDebt {
            debt_tokens: self.post_compaction_tokens as f64
                * self.incremental_cache_cost_ratio.unwrap_or(0.0),
            repayment_tokens: self.archive_tokens.saturating_sub(self.memo_tokens),
        }
    }
}

pub fn decide(input: &CompactionInput, economics: &CompactionEconomics) -> CompactionDecision {
    let horizon = input
        .completed_boundary_request_counts
        .as_deref()
        .map(|counts| {
            estimate_remaining_requests(&HorizonInput {
                completed_boundary_request_counts: counts,
                remaining_boundaries: input.remaining_boundaries,
                scale: economics.remaining_request_scale,
                standard_deviation_k: economics.remaining_request_stddev_k,
                context_tokens: input.context_tokens,
                context_window_tokens: input.context_window_tokens,
                average_context_token_increment: input.average_context_token_increment,
            })
        });
    let expected = horizon
        .as_ref()
        .map(|h| h.expected_remaining_requests as f64);

    let saving_tokens = input.archive_tokens as f64 - input.memo_tokens as f64;
    let post_compaction_tokens = (input.write_tokens as f64 - saving_tokens).max(0.0);
    let incremental_cache_cost_ratio = input
        .cache_write_read_ratio
        .map(|ratio| (ratio - 1.0).max(0.0));
    let new_debt_tokens = post_compaction_tokens * incremental_cache_cost_ratio.unwrap_or(0.0);
    let breakeven_requests = (saving_tokens > 0.0 && incremental_cache_cost_ratio.is_some())
        .then(|| new_debt_tokens / saving_tokens);
    let combined_repayment_tokens = input.cache_debt_repayment_tokens as f64 + saving_tokens;
    let combined_breakeven_requests = (saving_tokens > 0.0
        && incremental_cache_cost_ratio.is_some()
        && combined_repayment_tokens > 0.0)
        .then(|| (input.carried_debt_tokens + new_debt_tokens) / combined_repayment_tokens);

    let first_compaction = input.prior_compaction_count == 0;
    let effective_horizon_requests = horizon.as_ref().map(|h| {
        let expected = h.expected_remaining_requests as f64;
        if first_compaction {
            let window = h
                .window_request_upper_bound
                .map_or(f64::INFINITY, |upper| upper as f64);
            (expected * economics.first_compaction_request_scale).min(window)
        } else {
            expected
        }
    });

    let window_protection = input.context_window_tokens.is_some_and(|window| {
        input.context_tokens as f64 >= window as f64 - economics.window_reserve_tokens as f64
    });
    let within = |requests: Option<f64>, horizon: Option<f64>| match (requests, horizon) {
        (Some(requests), Some(horizon)) => requests <= horizon,
        _ => false,
    };
    let base_economic = expected.is_some_and(|e| e > 0.0) && within(breakeven_requests, expected);
    let first_economic = first_compaction
        && effective_horizon_requests.is_some_and(|e| e > 0.0)
        && within(breakeven_requests, effective_horizon_requests);
    let subsequent_margin_open = !first_compaction
        && within(
            breakeven_requests.map(|b| b * economics.subsequent_compaction_margin),
            expected,
        );
    let carried_debt_gate_open = !first_compaction && within(combined_breakeven_requests, expected);
    let economic = if first_compaction {
        first_economic
    } else {
        base_economic && subsequent_margin_open && carried_debt_gate_open
    };
    let compressible = saving_tokens > 0.0;
    let cooldown_active = input
        .requests_since_last_compaction
        .is_some_and(|since| since < economics.minimum_requests_since_compaction);
    // 窗口保护是绝对的：贴着窗口时冷却期里也压。
    let compact = compressible && (window_protection || (economic && !cooldown_active));

    let reason = if !compressible {
        CompactionReason::NonPositiveSaving
    } else if window_protection {
        CompactionReason::WindowProtection
    } else if economic && cooldown_active {
        CompactionReason::DeferredPostCompactionCooldown
    } else if economic {
        CompactionReason::Economic
    } else if horizon.is_none() {
        CompactionReason::HorizonUnavailable
    } else if breakeven_requests.is_none() {
        CompactionReason::CacheRatioUnavailable
    } else if !first_compaction && base_economic && !subsequent_margin_open {
        CompactionReason::DeferredSubsequentMargin
    } else if !first_compaction && base_economic && !carried_debt_gate_open {
        CompactionReason::DeferredCarriedDebt
    } else {
        CompactionReason::DeferredEconomic
    };

    CompactionDecision {
        write_tokens: input.write_tokens,
        archive_tokens: input.archive_tokens,
        memo_tokens: input.memo_tokens,
        post_compaction_tokens: post_compaction_tokens as u64,
        context_tokens: input.context_tokens,
        horizon,
        breakeven_requests,
        combined_breakeven_requests,
        effective_horizon_requests,
        cache_write_read_ratio: input.cache_write_read_ratio,
        incremental_cache_cost_ratio,
        prior_compaction_count: input.prior_compaction_count,
        requests_since_last_compaction: input.requests_since_last_compaction,
        carried_debt_tokens: input.carried_debt_tokens,
        cache_debt_repayment_tokens: input.cache_debt_repayment_tokens,
        compact,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn horizon(counts: &[u64], remaining: u64, k: f64) -> RequestHorizon {
        estimate_remaining_requests(&HorizonInput {
            completed_boundary_request_counts: counts,
            remaining_boundaries: remaining,
            scale: 1.0,
            standard_deviation_k: k,
            context_tokens: 0,
            context_window_tokens: None,
            average_context_token_increment: None,
        })
    }

    #[test]
    fn the_horizon_is_mean_requests_per_step_times_remaining_steps_plus_one() {
        let h = horizon(&[3, 5], 2, 0.0);
        assert_eq!(h.requests_per_boundary_mean, 4.0);
        assert_eq!(h.requests_per_boundary_lower_bound, 4.0);
        assert_eq!(h.unbounded_expected_remaining_requests, 9);
        assert_eq!(h.expected_remaining_requests, 9);
        assert_eq!(h.window_request_upper_bound, None);

        let empty = horizon(&[], 3, 0.0);
        assert_eq!(empty.requests_per_boundary_mean, 0.0);
        assert_eq!(
            empty.expected_remaining_requests, 1,
            "没有样本也至少还有这一次"
        );
    }

    #[test]
    fn a_stddev_k_halves_small_samples_and_subtracts_deviation_from_larger_ones() {
        // 两个样本：均值 5，打对折 2.5，× 2 步 = 5 → 1 + 5 = 6。
        let small = horizon(&[4, 6], 2, 1.0);
        assert_eq!(small.requests_per_boundary_lower_bound, 2.5);
        assert_eq!(small.unbounded_expected_remaining_requests, 6);

        // [2,4,6,8]：均值 5，样本方差 20/3，标准差 ≈ 2.582，下界 ≈ 2.418，
        // × 2 步 ≈ 4.836 → 1 + 4 = 5。
        let large = horizon(&[2, 4, 6, 8], 2, 1.0);
        let deviation = (20.0f64 / 3.0).sqrt();
        assert!((large.requests_per_boundary_lower_bound - (5.0 - deviation)).abs() < 1e-12);
        assert_eq!(large.unbounded_expected_remaining_requests, 5);

        // 下界不会是负的。
        let wide = horizon(&[1, 1, 30], 4, 3.0);
        assert_eq!(wide.requests_per_boundary_lower_bound, 0.0);
        assert_eq!(wide.unbounded_expected_remaining_requests, 1);
    }

    #[test]
    fn the_window_caps_the_horizon() {
        let input = |context| HorizonInput {
            completed_boundary_request_counts: &[10],
            remaining_boundaries: 3,
            scale: 1.0,
            standard_deviation_k: 0.0,
            context_tokens: context,
            context_window_tokens: Some(100_000),
            average_context_token_increment: Some(300.0),
        };
        // ⌊1000 / 300⌋ = 3 < 31。
        let h = estimate_remaining_requests(&input(99_000));
        assert_eq!(h.unbounded_expected_remaining_requests, 31);
        assert_eq!(h.window_request_upper_bound, Some(3));
        assert_eq!(h.expected_remaining_requests, 3);
        // 已经超过窗口：上界是 0，不是负数。
        let over = estimate_remaining_requests(&input(120_000));
        assert_eq!(over.window_request_upper_bound, Some(0));
        assert_eq!(over.expected_remaining_requests, 0);
        // 涨幅不是正数就没有上界。
        let flat = estimate_remaining_requests(&HorizonInput {
            average_context_token_increment: Some(0.0),
            ..input(99_000)
        });
        assert_eq!(flat.window_request_upper_bound, None);
    }

    /// 第一次压缩的基准场景：账算得过来。
    ///
    /// saving = 60000 - 1000 = 59000；post = 100000 - 59000 = 41000；
    /// 增量比 = 1.25 - 1 = 0.25；新债 = 10250；回本 = 10250 / 59000 ≈ 0.174。
    /// 每步 [3,5] 均值 4 × 剩 2 步 → 9；窗口上界 ⌊(200000-100000)/5000⌋ = 20；
    /// 第一次放大到 min(18, 20) = 18。
    fn base() -> CompactionInput {
        CompactionInput {
            write_tokens: 100_000,
            archive_tokens: 60_000,
            memo_tokens: 1_000,
            context_tokens: 100_000,
            completed_boundary_request_counts: Some(vec![3, 5]),
            remaining_boundaries: 2,
            average_context_token_increment: Some(5_000.0),
            context_window_tokens: Some(200_000),
            prior_compaction_count: 0,
            requests_since_last_compaction: None,
            carried_debt_tokens: 0.0,
            cache_debt_repayment_tokens: 0,
            cache_write_read_ratio: Some(1.25),
        }
    }

    /// 之后的压缩：每步 [1] × 剩 1 步 → 剩余 2 次请求，不设窗口。
    fn subsequent() -> CompactionInput {
        CompactionInput {
            write_tokens: 20_000,
            archive_tokens: 11_000,
            memo_tokens: 1_000,
            context_tokens: 20_000,
            completed_boundary_request_counts: Some(vec![1]),
            remaining_boundaries: 1,
            average_context_token_increment: None,
            context_window_tokens: None,
            prior_compaction_count: 1,
            requests_since_last_compaction: Some(5),
            carried_debt_tokens: 0.0,
            cache_debt_repayment_tokens: 0,
            cache_write_read_ratio: Some(1.25),
        }
    }

    #[test]
    fn the_first_compaction_numbers_match_the_formulas() {
        let d = decide(&base(), &CompactionEconomics::default());
        assert_eq!(d.post_compaction_tokens, 41_000);
        assert_eq!(d.incremental_cache_cost_ratio, Some(0.25));
        assert_eq!(d.breakeven_requests, Some(10_250.0 / 59_000.0));
        assert_eq!(d.combined_breakeven_requests, Some(10_250.0 / 59_000.0));
        let h = d.horizon.as_ref().unwrap();
        assert_eq!(h.expected_remaining_requests, 9);
        assert_eq!(h.window_request_upper_bound, Some(20));
        assert_eq!(d.effective_horizon_requests, Some(18.0));
        assert!(d.compact);
        assert_eq!(d.reason, CompactionReason::Economic);
        assert_eq!(
            d.debt(),
            CompactionDebt {
                debt_tokens: 10_250.0,
                repayment_tokens: 59_000
            }
        );
    }

    #[test]
    fn every_reason_branch() {
        let economics = CompactionEconomics::default();
        let cases: Vec<(&str, CompactionInput, bool, CompactionReason)> = vec![
            (
                "第一次，账算得过来",
                base(),
                true,
                CompactionReason::Economic,
            ),
            (
                // 剩余 1 次 × 2 = 2；新债 = 10000 × 4 = 40000，回本 4 > 2。
                "第一次，账算不过来",
                CompactionInput {
                    completed_boundary_request_counts: Some(vec![]),
                    context_window_tokens: None,
                    cache_write_read_ratio: Some(5.0),
                    ..subsequent()
                }
                .first(),
                false,
                CompactionReason::DeferredEconomic,
            ),
            (
                // 增量比 1.5，新债 15000，回本 1.5 ≤ 2，但 1.5 × 1.5 = 2.25 > 2。
                "之后的压缩，margin 不够",
                CompactionInput {
                    cache_write_read_ratio: Some(2.5),
                    ..subsequent()
                },
                false,
                CompactionReason::DeferredSubsequentMargin,
            ),
            (
                // 剩余 [4] × 1 → 5；回本 2500/10000 = 0.25，margin 0.375 ≤ 5；
                // 连旧债：(100000 + 2500) / 10000 = 10.25 > 5。
                "之后的压缩，旧债还不清",
                CompactionInput {
                    completed_boundary_request_counts: Some(vec![4]),
                    carried_debt_tokens: 100_000.0,
                    ..subsequent()
                },
                false,
                CompactionReason::DeferredCarriedDebt,
            ),
            (
                // 同 base 但已压过一次：剩余 9（不放大），回本 0.174、margin 0.26、
                // 连旧债 0.174 都 ≤ 9——账算得过来，可上次压缩才过去 1 次请求。
                "冷却期",
                CompactionInput {
                    prior_compaction_count: 1,
                    requests_since_last_compaction: Some(1),
                    ..base()
                },
                false,
                CompactionReason::DeferredPostCompactionCooldown,
            ),
            (
                // 190000 ≥ 200000 - 16384 = 183616。
                "窗口保护压过冷却期",
                CompactionInput {
                    prior_compaction_count: 1,
                    requests_since_last_compaction: Some(1),
                    context_tokens: 190_000,
                    ..base()
                },
                true,
                CompactionReason::WindowProtection,
            ),
            (
                "窗口保护不需要剩余请求估计",
                CompactionInput {
                    completed_boundary_request_counts: None,
                    cache_write_read_ratio: None,
                    context_tokens: 190_000,
                    ..base()
                },
                true,
                CompactionReason::WindowProtection,
            ),
            (
                "摘要不比原文短，贴着窗口也不压",
                CompactionInput {
                    archive_tokens: 1_000,
                    context_tokens: 190_000,
                    ..base()
                },
                false,
                CompactionReason::NonPositiveSaving,
            ),
            (
                "不知道缓存价格比",
                CompactionInput {
                    cache_write_read_ratio: None,
                    ..base()
                },
                false,
                CompactionReason::CacheRatioUnavailable,
            ),
            (
                "估不出剩余请求",
                CompactionInput {
                    completed_boundary_request_counts: None,
                    ..base()
                },
                false,
                CompactionReason::HorizonUnavailable,
            ),
        ];
        for (name, input, compact, reason) in cases {
            let d = decide(&input, &economics);
            assert_eq!((d.compact, d.reason), (compact, reason), "{name}：{d:#?}");
        }
    }

    #[test]
    fn the_cooldown_only_holds_back_an_economic_compaction() {
        let d = decide(
            &CompactionInput {
                requests_since_last_compaction: Some(2),
                prior_compaction_count: 1,
                ..base()
            },
            &CompactionEconomics::default(),
        );
        assert_eq!((d.compact, d.reason), (true, CompactionReason::Economic));
    }

    #[test]
    fn a_compaction_with_nothing_to_cut_is_marked_not_compactable() {
        let economics = CompactionEconomics::default();
        let d = decide(&base(), &economics).mark_not_compactable();
        assert_eq!(
            (d.compact, d.reason),
            (false, CompactionReason::NativeNotCompactable)
        );

        let deferred = decide(
            &CompactionInput {
                cache_write_read_ratio: None,
                ..base()
            },
            &economics,
        )
        .mark_not_compactable();
        assert_eq!(deferred.reason, CompactionReason::CacheRatioUnavailable);
    }

    #[test]
    fn reasons_serialize_in_snake_case() {
        assert_eq!(
            serde_json::to_value(CompactionReason::DeferredPostCompactionCooldown).unwrap(),
            "deferred_post_compaction_cooldown"
        );
        let d = decide(&base(), &CompactionEconomics::default());
        let back: CompactionDecision =
            serde_json::from_value(serde_json::to_value(&d).unwrap()).unwrap();
        assert_eq!(back, d);
    }

    impl CompactionInput {
        fn first(self) -> Self {
            CompactionInput {
                prior_compaction_count: 0,
                requests_since_last_compaction: None,
                ..self
            }
        }
    }
}
