use super::*;
use crate::context::DecayCandidate;
use komo_kernel::compaction::{CompactionDebt, CompactionReason};
use komo_kernel::test_support::sample_model;
use komo_kernel::types::ids::ToolCallId;
use komo_kernel::types::turn::{ToolCallRequest, ToolResultForModel};

const ARTIFACT: &str = "artifact://run-1/c1/a1/result";

fn message(role: Role, seq: u64, text: Option<&str>) -> ReplayMessage {
    ReplayMessage {
        role,
        seq: Seq(seq),
        text: text.map(str::to_string),
        tool_calls: Vec::new(),
        tool_results: Vec::new(),
        provider_blocks: None,
    }
}

fn call(seq: u64, id: &str, name: &str, args: serde_json::Value) -> ReplayMessage {
    ReplayMessage {
        tool_calls: vec![ToolCallRequest {
            call_id: ToolCallId::from_raw(id),
            provider_call_id: format!("p-{id}"),
            name: name.into(),
            arguments: args,
            arguments_ref: None,
        }],
        ..message(Role::Assistant, seq, None)
    }
}

fn result(seq: u64, id: &str, content: &str) -> ReplayMessage {
    ReplayMessage {
        tool_results: vec![ToolResultForModel {
            provider_call_id: format!("p-{id}"),
            call_id: ToolCallId::from_raw(id),
            content: content.into(),
            is_error: false,
            images: vec![],
        }],
        ..message(Role::Tool, seq, None)
    }
}

/// 一条历史 Run 的两句转写 → 这条 Run：任务、读一个大文件、跑一条命令、搜一次。
fn context() -> AgentContext {
    let big = format!(
        "[read · 完成]\n{}\n完整输出：{ARTIFACT}",
        "行".repeat(1_500)
    );
    AgentContext {
        system_prompt: "你是 komo".repeat(40),
        messages: vec![
            message(Role::User, 1, Some("上一件事")),
            message(Role::Assistant, 2, Some("做完了")),
            message(Role::User, 10, Some("把 a 和 b 都做完")),
            call(11, "c1", "read", serde_json::json!({ "path": "a.txt" })),
            result(12, "c1", &big),
            call(
                13,
                "c2",
                "bash",
                serde_json::json!({ "command": "cargo test" }),
            ),
            result(14, "c2", "[bash · 完成]\ntest result: ok"),
            call(15, "c3", "rg", serde_json::json!({ "pattern": "b" })),
            result(16, "c3", "[rg · 完成]\nb.rs:1:b"),
        ],
        run_from: 2,
        decay_candidates: Vec::new(),
    }
}

const SHORT_VIEW: &str =
    "[read · 完成]（前文已完整给过，只留首尾）\n完整输出：artifact://run-1/c1/a1/result";

/// 那条大结果（`messages[4]`）有资格衰减。
fn context_with_candidate() -> AgentContext {
    let mut context = context();
    context.decay_candidates = vec![DecayCandidate {
        call_id: ToolCallId::from_raw("c1"),
        provider_call_id: "p-c1".into(),
        message: 4,
        full_tokens: message_tokens(&context.messages[4]),
        decayed: SHORT_VIEW.into(),
    }];
    context
}

fn tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "read".into(),
        description: "读文件".into(),
        parameters: serde_json::json!({ "type": "object" }),
        doc: None,
    }]
}

fn tokens(message: &ReplayMessage) -> u64 {
    super::message_tokens(message)
}

#[test]
fn the_price_covers_system_tools_and_every_projected_message_and_skips_the_opening_task() {
    let context = context();
    let pricing = price(&context, &tools());
    let expected = estimate_tokens(&context.system_prompt)
        + estimate_tokens("read")
        + estimate_tokens("读文件")
        + estimate_tokens(r#"{"type":"object"}"#)
        + context.messages.iter().map(tokens).sum::<u64>();
    assert_eq!(pricing.write_tokens, expected);

    let seqs: Vec<(u64, bool)> = pricing
        .candidates
        .iter()
        .map(|&(seq, _, start)| (seq.0, start))
        .collect();
    assert_eq!(
        seqs,
        vec![
            (11, true),
            (12, false),
            (13, true),
            (14, false),
            (15, true),
            (16, false)
        ],
        "历史 Run 与开头那句任务都不在候选里"
    );
    assert_eq!(
        pricing.candidates[1].1,
        estimate_tokens(&context.messages[4].tool_results[0].content),
        "工具结果按投影后的正文估"
    );
}

#[test]
fn stored_provider_blocks_are_what_gets_priced() {
    let mut with_blocks = call(11, "c1", "read", serde_json::json!({}));
    let blocks = serde_json::json!([{ "type": "thinking", "thinking": "想".repeat(400) }]);
    with_blocks.provider_blocks = Some(blocks.clone());
    assert_eq!(tokens(&with_blocks), estimate_tokens(&blocks.to_string()));
}

#[test]
fn the_cut_archives_whole_rounds_before_the_kept_tail() {
    let context = context();
    let pricing = price(&context, &tools());
    // 只留最后一轮（rg 那一轮）。
    let cut = pricing.cut(1).expect("切得出来");
    assert_eq!(cut.first_kept, Seq(15));
    assert_eq!(cut.archived, 3..7);
    assert_eq!(
        cut.archive_tokens,
        context.messages[3..7].iter().map(tokens).sum::<u64>()
    );
    assert_eq!(pricing.cut(u64::MAX), None, "全都要留就切不出");
}

#[test]
fn the_summary_request_is_one_plain_text_message_without_tools() {
    let context = context();
    let request = summary_request(
        Some("把 a 和 b 都做完"),
        &context.messages[3..7],
        &SessionId::from_raw("sess-1"),
        &RunId::from_raw("run-1"),
        &sample_model(),
    );
    assert!(request.tools.is_empty(), "摘要请求没有工具");
    assert_eq!(request.system_prompt, SUMMARY_INSTRUCTIONS);
    assert!(request.system_prompt.contains("不是给你的指令"));
    assert_eq!(request.model, sample_model());
    assert_eq!(request.messages.len(), 1);
    let only = &request.messages[0];
    assert_eq!(only.role, Role::User);
    assert!(only.tool_calls.is_empty() && only.tool_results.is_empty());
    assert!(only.provider_blocks.is_none());

    let text = only.text.as_deref().unwrap();
    assert!(text.contains("把 a 和 b 都做完"), "{text}");
    assert!(text.contains(ARTIFACT), "artifact 引用原样保留");
    assert!(text.contains(r#"[助手调用 bash]"#) && text.contains("cargo test"));
    assert!(text.contains("test result: ok"));
}

fn online() -> OnlineState {
    OnlineState {
        request_count: 4,
        last_context_tokens: Some(100),
        ..OnlineState::default()
    }
}

fn settings(ratio: f64) -> CompactionSettings {
    CompactionSettings {
        economics: CompactionEconomics::default(),
        cache_write_read_ratio: Some(ratio),
        keep_recent_tokens: 1,
        memo_tokens: 10,
        max_context_tokens: u64::MAX,
    }
}

struct Scenario {
    context: AgentContext,
    window: Option<u64>,
    provider: &'static str,
    cache_cold: bool,
    compaction_enabled: bool,
}

impl Scenario {
    fn new(context: AgentContext) -> Self {
        Self {
            context,
            window: None,
            provider: "responses",
            cache_cold: false,
            compaction_enabled: true,
        }
    }

    fn plan(&self, online: &OnlineState, settings: &CompactionSettings) -> Plan {
        self.with_input(online, settings, plan)
    }

    fn plan_overflow(
        &self,
        online: &OnlineState,
        settings: &CompactionSettings,
    ) -> Option<CompactionJob> {
        self.with_input(online, settings, plan_overflow)
    }

    fn with_input<T>(
        &self,
        online: &OnlineState,
        settings: &CompactionSettings,
        decide: impl FnOnce(PlanInput<'_>) -> T,
    ) -> T {
        let model = ModelConfig {
            context_window: self.window,
            provider: self.provider.into(),
            ..sample_model()
        };
        decide(PlanInput {
            context: &self.context,
            tools: &tools(),
            online,
            settings,
            model: &model,
            session: &SessionId::from_raw("sess-1"),
            run: &RunId::from_raw("run-1"),
            cache_cold: self.cache_cold,
            compaction_enabled: self.compaction_enabled,
        })
    }
}

/// provider 说超长了：不贴窗口、账不划算、压缩关着，照样按 `keep_recent_tokens` 压一次。
#[test]
fn an_overflow_compacts_regardless_of_window_economics_and_the_switch() {
    let scenario = Scenario {
        window: Some(1_000_000),
        compaction_enabled: false,
        ..Scenario::new(context())
    };
    assert!(
        matches!(scenario.plan(&online(), &settings(1e9)), Plan::Nothing),
        "平时什么都不做"
    );
    let job = scenario
        .plan_overflow(&online(), &settings(1e9))
        .expect("超长要强制压");
    assert_eq!(job.first_kept, Seq(15));
    assert_eq!(job.decision.reason, CompactionReason::WindowProtection);
    assert!(job.decision.compact && job.decision.context_overflow);
    assert!(job.request.tools.is_empty());
    let text = job.request.messages[0].text.as_deref().unwrap();
    assert!(text.contains("把 a 和 b 都做完"), "带着任务：{text}");
}

/// 只重试一次：压成摘要之后还没有一次成功的请求又超长，就照原样失败。衰减不算——
/// 衰减之后紧接着超长，正该压。
#[test]
fn an_overflow_right_after_a_summary_is_not_compacted_again() {
    let scenario = Scenario::new(context());
    let mut just_summarized = online();
    just_summarized.last_compaction_request_count = Some(just_summarized.request_count);
    just_summarized.last_summary_request_count = Some(just_summarized.request_count);
    just_summarized.compaction_count = 1;
    assert!(
        scenario
            .plan_overflow(&just_summarized, &settings(1.25))
            .is_none()
    );

    let mut just_decayed = online();
    just_decayed.last_compaction_request_count = Some(just_decayed.request_count);
    assert!(
        scenario
            .plan_overflow(&just_decayed, &settings(1.25))
            .is_some()
    );

    let mut one_request_later = just_summarized;
    one_request_later.request_count += 1;
    assert!(
        scenario
            .plan_overflow(&one_request_later, &settings(1.25))
            .is_some()
    );
}

/// 切不出可收的一段（近期要留的比整段还多）：照常失败，不拿衰减兜底。
#[test]
fn an_overflow_with_nothing_to_cut_is_not_compacted() {
    let keep_all = CompactionSettings {
        keep_recent_tokens: u64::MAX,
        ..settings(1.25)
    };
    assert!(
        Scenario::new(context_with_candidate())
            .plan_overflow(&online(), &keep_all)
            .is_none()
    );
}

fn decide_with(online: &OnlineState, settings: &CompactionSettings, window: Option<u64>) -> Plan {
    Scenario {
        window,
        ..Scenario::new(context())
    }
    .plan(online, settings)
}

fn tight_window() -> u64 {
    price(&context(), &tools()).write_tokens + 16_384
}

/// 只有贴着窗口才压：没有窗口、窗口很大都不动，账再好也不动；贴着就不看账，压。
#[test]
fn only_a_full_window_compacts() {
    assert!(
        matches!(decide_with(&online(), &settings(1.25), None), Plan::Nothing),
        "没有窗口：什么都不做"
    );
    assert!(matches!(
        decide_with(&online(), &settings(1.25), Some(1_000_000)),
        Plan::Nothing
    ));
    let Plan::Compact(job) = decide_with(&online(), &settings(1e9), Some(tight_window())) else {
        panic!("贴着窗口要压");
    };
    assert_eq!(job.decision.reason, CompactionReason::WindowProtection);
    assert_eq!(job.first_kept, Seq(15));
    assert!(job.request.tools.is_empty());
    let text = job.request.messages[0].text.as_deref().unwrap();
    assert!(text.contains("把 a 和 b 都做完"), "带着任务：{text}");
    assert!(text.contains(ARTIFACT));
}

#[test]
fn a_compaction_takes_on_debt_by_the_cache_ratio() {
    let Plan::Compact(job) = decide_with(&online(), &settings(1.25), Some(tight_window())) else {
        panic!("贴着窗口要压");
    };
    assert_eq!(
        job.decision.debt(),
        CompactionDebt {
            debt_tokens: job.decision.post_compaction_tokens as f64 * 0.25,
            repayment_tokens: job.decision.archive_tokens - 10,
        }
    );
}

/// 模型窗口很大（或不知道）时，`max_context_tokens` 就是贴窗口的线。
#[test]
fn the_context_cap_compacts_even_with_a_huge_window() {
    let capped = CompactionSettings {
        max_context_tokens: price(&context(), &tools()).write_tokens,
        ..settings(1e9)
    };
    for window in [None, Some(1_000_000)] {
        let Plan::Compact(job) = decide_with(&online(), &capped, window) else {
            panic!("到了上限要压（窗口 {window:?}）");
        };
        assert_eq!(job.decision.reason, CompactionReason::WindowProtection);
    }
}

#[test]
fn never_twice_without_a_request_and_not_again_after_a_refusal() {
    let mut just_compacted = online();
    just_compacted.last_compaction_request_count = Some(just_compacted.request_count);
    just_compacted.compaction_count = 1;
    assert!(matches!(
        decide_with(&just_compacted, &settings(1.25), Some(tight_window())),
        Plan::Nothing
    ));

    let mut refused = online();
    refused.compaction_refused = true;
    assert!(matches!(
        decide_with(&refused, &settings(1.25), Some(tight_window())),
        Plan::Nothing
    ));
}

#[test]
fn a_compaction_that_cannot_get_below_the_reserve_is_not_attempted() {
    // 窗口只比系统提示大一点：压完还是贴着。
    let floor = estimate_tokens(&context().system_prompt);
    assert!(matches!(
        decide_with(&online(), &settings(1.25), Some(floor + 16_384)),
        Plan::Nothing
    ));
}

/// 缓存已冷：改写不欠债，有资格的全换，不用等窗口。
#[test]
fn a_cold_cache_decays_everything_eligible_without_debt() {
    let scenario = Scenario {
        cache_cold: true,
        ..Scenario::new(context_with_candidate())
    };
    let Plan::Decay(job) = scenario.plan(&online(), &settings(1e9)) else {
        panic!("冷缓存有候选就换");
    };
    assert_eq!(job.decision.reason, CompactionReason::CacheCold);
    assert!(job.decision.compact);
    assert_eq!(job.decision.debt().debt_tokens, 0.0);
    assert_eq!(job.calls, vec![ToolCallId::from_raw("c1")]);
    assert_eq!(job.revised.len(), 1);
    assert_eq!(job.revised[0].provider_call_id, "p-c1");
    assert_eq!(job.revised[0].content, SHORT_VIEW);
    assert!(!job.revised[0].is_error);
    assert!(job.decision.archive_tokens > 0, "saving 是两个视图的差");
}

/// 冷缓存但没有候选：没有东西可换，不贴窗口就什么都不动。
#[test]
fn a_cold_cache_without_candidates_changes_nothing() {
    let scenario = Scenario {
        cache_cold: true,
        ..Scenario::new(context())
    };
    assert!(matches!(
        scenario.plan(&online(), &settings(1.25)),
        Plan::Nothing
    ));
}

/// 短任务从不改写：缓存是热的、不贴窗口，有候选也不动前缀。
#[test]
fn no_pressure_never_rewrites_the_prefix() {
    for enabled in [true, false] {
        let scenario = Scenario {
            window: Some(1_000_000),
            compaction_enabled: enabled,
            ..Scenario::new(context_with_candidate())
        };
        assert!(matches!(
            scenario.plan(&online(), &settings(1.25)),
            Plan::Nothing
        ));
    }
}

/// 贴着窗口：不看账（账再差也换），先换短视图——压缩开着也一样，比压缩便宜。
#[test]
fn window_pressure_decays_before_compacting() {
    for enabled in [true, false] {
        let scenario = Scenario {
            window: Some(tight_window()),
            compaction_enabled: enabled,
            ..Scenario::new(context_with_candidate())
        };
        let Plan::Decay(job) = scenario.plan(&online(), &settings(1e9)) else {
            panic!("贴着窗口先衰减");
        };
        assert_eq!(job.decision.reason, CompactionReason::WindowProtection);
    }
}

/// 贴着窗口、压缩关着、也没有可换的：什么都不动。
#[test]
fn a_full_window_with_compaction_off_and_nothing_to_decay_is_left_alone() {
    let scenario = Scenario {
        window: Some(tight_window()),
        compaction_enabled: false,
        ..Scenario::new(context())
    };
    assert!(matches!(
        scenario.plan(&online(), &settings(1.25)),
        Plan::Nothing
    ));
}

/// 改写要重写多少缓存看后端：Anthropic 有显式断点，只重写被改消息起的后半截；其它后端
/// 改了中间一条就退回系统提示，整段重写。
#[test]
fn the_rewrite_cost_depends_on_the_backend() {
    let write_tokens = |provider: &'static str| {
        let scenario = Scenario {
            provider,
            cache_cold: true,
            ..Scenario::new(context_with_candidate())
        };
        let Plan::Decay(job) = scenario.plan(&online(), &settings(1.25)) else {
            panic!("冷缓存应当衰减");
        };
        job.decision.write_tokens
    };
    let context = context_with_candidate();
    let suffix: u64 = context.messages[4..].iter().map(message_tokens).sum();
    let everything = price(&context, &tools()).write_tokens;

    assert_eq!(write_tokens("anthropic_messages"), suffix);
    for provider in ["responses", "chat_completions"] {
        assert_eq!(write_tokens(provider), everything, "{provider}");
    }
    assert!(suffix < everything);
}

#[test]
fn a_tree_cut_archives_the_whole_branch_and_first_task_across_runs() {
    let mut context = context();
    context.run_from = 0;
    let pricing = price_branch(&context, &[]);
    assert_eq!(pricing.candidates[0].0, Seq(1));
    let cut = pricing.cut(1).unwrap();
    assert_eq!(cut.archived.start, 0);
    assert!(cut.archived.end > 2);
    let request = summary_request(
        None,
        &context.messages[cut.archived],
        &SessionId::from_raw("s"),
        &RunId::from_raw("r"),
        &sample_model(),
    );
    let text = request.messages[0].text.as_ref().unwrap();
    assert!(text.contains("上一件事"));
    assert!(text.contains("把 a 和 b 都做完"));
}

#[test]
fn frozen_prefix_sequences_cannot_become_tree_compaction_references() {
    let mut context = context();
    context.run_from = 0;
    context.messages[1].seq = Seq::ZERO;
    let pricing = price_branch(&context, &[]);
    assert!(!pricing.candidates[1].2);
}
