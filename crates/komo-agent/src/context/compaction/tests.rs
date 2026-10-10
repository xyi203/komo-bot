use super::*;
use crate::context::DecayCandidate;
use komo_kernel::compaction::{CompactionDebt, PlanStatus};
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
        }],
        ..message(Role::Tool, seq, None)
    }
}

/// 一条历史 Run 的两句转写 → 这条 Run：任务、读一个大文件、跑一条命令、报一次计划。
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
                "shell",
                serde_json::json!({ "command": "cargo test" }),
            ),
            result(14, "c2", "[shell · 完成]\ntest result: ok"),
            call(15, "c3", "update_plan", serde_json::json!({ "steps": [] })),
            result(16, "c3", "<komo-plan/>"),
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
    // 只留最后一轮（update_plan 那一轮）。
    let cut = pricing.cut(1).expect("切得出来");
    assert_eq!(cut.first_kept, Seq(15));
    assert_eq!(cut.archived, 3..7);
    assert_eq!(
        cut.archive_tokens,
        context.messages[3..7].iter().map(tokens).sum::<u64>()
    );
    assert_eq!(pricing.cut(u64::MAX), None, "全都要留就切不出");
}

fn plan_steps() -> Vec<PlanStep> {
    vec![
        PlanStep {
            id: "a".into(),
            goal: "做 a".into(),
            status: PlanStatus::Completed,
        },
        PlanStep {
            id: "b".into(),
            goal: "做 b".into(),
            status: PlanStatus::InProgress,
        },
    ]
}

fn progress() -> Vec<ProgressSummary> {
    vec![ProgressSummary {
        step_id: "a".into(),
        goal: "做 a".into(),
        files_changed: vec!["src/a.rs".into()],
        verification: vec!["cargo test 通过".into()],
        decisions: vec!["用 BTreeMap".into()],
        next_work: vec!["做 b".into()],
    }]
}

#[test]
fn the_summary_request_is_one_plain_text_message_without_tools() {
    let context = context();
    let request = summary_request(
        Some("把 a 和 b 都做完"),
        &context.messages[3..7],
        &plan_steps(),
        &progress(),
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
    assert!(
        text.contains(&format_snapshot(&plan_steps())),
        "带着当前计划"
    );
    for piece in ["src/a.rs", "cargo test 通过", "用 BTreeMap"] {
        assert!(text.contains(piece), "带着进度：{piece}");
    }
    assert!(text.contains(ARTIFACT), "artifact 引用原样保留");
    assert!(text.contains(r#"[助手调用 shell]"#) && text.contains("cargo test"));
    assert!(text.contains("test result: ok"));
}

fn online(boundary: bool) -> OnlineState {
    OnlineState {
        plan: plan_steps(),
        pending_progress: progress(),
        request_count: 4,
        last_boundary_request_count: 4,
        completed_boundary_request_counts: vec![3],
        last_context_tokens: Some(100),
        pending_boundary: boundary,
        ..OnlineState::default()
    }
}

fn settings(ratio: f64) -> CompactionSettings {
    CompactionSettings {
        economics: CompactionEconomics::default(),
        cache_write_read_ratio: Some(ratio),
        keep_recent_tokens: 1,
        memo_tokens: 10,
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
        let model = ModelConfig {
            context_window: self.window,
            provider: self.provider.into(),
            ..sample_model()
        };
        plan(PlanInput {
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

#[test]
fn a_profitable_boundary_compacts_and_carries_the_plan_into_the_summary_request() {
    let Plan::Compact(job) = decide_with(&online(true), &settings(1.25), None) else {
        panic!("划算的边界应当压");
    };
    assert_eq!(job.first_kept, Seq(15));
    assert_eq!(job.decision.reason, CompactionReason::Economic);
    assert!(job.request.tools.is_empty());
    let text = job.request.messages[0].text.as_deref().unwrap();
    assert!(text.contains(&format_snapshot(&plan_steps())));
    assert!(text.contains(ARTIFACT));
    assert_eq!(
        job.decision.debt(),
        CompactionDebt {
            debt_tokens: job.decision.post_compaction_tokens as f64 * 0.25,
            repayment_tokens: job.decision.archive_tokens - 10,
        }
    );
}

#[test]
fn an_unprofitable_boundary_is_skipped_with_its_decision() {
    let Plan::Skip { reason, decision } = decide_with(&online(true), &settings(1e9), None) else {
        panic!("算不过来的边界要记 skipped");
    };
    assert_eq!(reason, "deferred_economic");
    assert!(!decision.compact);
}

#[test]
fn off_a_boundary_only_the_window_compacts() {
    let economic = settings(1.25);
    assert!(
        matches!(decide_with(&online(false), &economic, None), Plan::Nothing),
        "不在边界、没有窗口：什么都不做"
    );
    // 窗口很大：不贴着，就算账算得过来也不在边界之间压。
    assert!(matches!(
        decide_with(&online(false), &economic, Some(1_000_000)),
        Plan::Nothing
    ));
    // 窗口小到贴着：不看账，压。
    let tight = price(&context(), &tools()).write_tokens + 16_384;
    let Plan::Compact(job) = decide_with(&online(false), &settings(1e9), Some(tight)) else {
        panic!("贴着窗口要压");
    };
    assert_eq!(job.decision.reason, CompactionReason::WindowProtection);
}

#[test]
fn never_twice_without_a_request_and_not_again_after_a_refusal_until_a_boundary() {
    let tight = price(&context(), &tools()).write_tokens + 16_384;
    let mut just_compacted = online(false);
    just_compacted.last_compaction_request_count = Some(just_compacted.request_count);
    just_compacted.compaction_count = 1;
    assert!(matches!(
        decide_with(&just_compacted, &settings(1.25), Some(tight)),
        Plan::Nothing
    ));

    let mut refused = online(false);
    refused.compaction_refused = true;
    assert!(matches!(
        decide_with(&refused, &settings(1.25), Some(tight)),
        Plan::Nothing
    ));
    refused.pending_boundary = true;
    assert!(
        matches!(
            decide_with(&refused, &settings(1.25), Some(tight)),
            Plan::Compact(_)
        ),
        "新的边界照常决策"
    );
}

#[test]
fn a_compaction_that_cannot_get_below_the_reserve_is_not_attempted() {
    // 窗口只比系统提示大一点：压完还是贴着。
    let floor = estimate_tokens(&context().system_prompt);
    assert!(matches!(
        decide_with(&online(false), &settings(1.25), Some(floor + 16_384)),
        Plan::Nothing
    ));
    let Plan::Skip { reason, decision } =
        decide_with(&online(true), &settings(1.25), Some(floor + 16_384))
    else {
        panic!("边界上要记 skipped");
    };
    assert!(reason.starts_with("压完仍贴着窗口"), "{reason}");
    assert!(!decision.compact);
}

/// 缓存已冷：改写不欠债，有资格的全换，连计划边界与窗口都不用等。
#[test]
fn a_cold_cache_decays_everything_eligible_without_debt() {
    let scenario = Scenario {
        cache_cold: true,
        ..Scenario::new(context_with_candidate())
    };
    let Plan::Decay(job) = scenario.plan(&online(false), &settings(1e9)) else {
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

/// 冷缓存但没有候选：没有东西可换，不在边界也不贴窗口就什么都不动。
#[test]
fn a_cold_cache_without_candidates_changes_nothing() {
    let scenario = Scenario {
        cache_cold: true,
        ..Scenario::new(context())
    };
    assert!(matches!(
        scenario.plan(&online(false), &settings(1.25)),
        Plan::Nothing
    ));
}

/// 短任务从不改写：缓存是热的、不在边界、不贴窗口，有候选也不动前缀。
#[test]
fn no_boundary_no_pressure_never_rewrites_the_prefix() {
    for enabled in [true, false] {
        let scenario = Scenario {
            window: Some(1_000_000),
            compaction_enabled: enabled,
            ..Scenario::new(context_with_candidate())
        };
        assert!(matches!(
            scenario.plan(&online(false), &settings(1.25)),
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
        let Plan::Decay(job) = scenario.plan(&online(false), &settings(1e9)) else {
            panic!("贴着窗口先衰减");
        };
        assert_eq!(job.decision.reason, CompactionReason::WindowProtection);
    }
}

/// 边界上衰减划算：压缩关着就衰减。
#[test]
fn a_profitable_boundary_decays_when_compaction_is_off() {
    let scenario = Scenario {
        compaction_enabled: false,
        ..Scenario::new(context_with_candidate())
    };
    let Plan::Decay(job) = scenario.plan(&online(true), &settings(1.25)) else {
        panic!("划算的边界应当衰减");
    };
    assert_eq!(job.decision.reason, CompactionReason::Economic);
    assert_eq!(job.decision.memo_tokens, 0);
}

/// 边界上压缩与衰减都划算：压缩优先。
#[test]
fn a_boundary_prefers_compaction_when_it_pays() {
    let scenario = Scenario::new(context_with_candidate());
    assert!(matches!(
        scenario.plan(&online(true), &settings(1.25)),
        Plan::Compact(_)
    ));
}

/// 边界上压缩不划算、衰减也不划算：记一条 skipped，带的是压缩那份账；压缩关着就带衰减的。
#[test]
fn a_boundary_where_nothing_pays_is_skipped() {
    let scenario = Scenario::new(context_with_candidate());
    let Plan::Skip { reason, decision } = scenario.plan(&online(true), &settings(1e9)) else {
        panic!("都算不过来的边界要记 skipped");
    };
    assert_eq!(reason, "deferred_economic");
    assert!(!decision.compact);
    assert!(decision.archive_tokens > 0);
    let compaction_archive = decision.archive_tokens;

    let scenario = Scenario {
        compaction_enabled: false,
        ..Scenario::new(context_with_candidate())
    };
    let Plan::Skip { reason, decision } = scenario.plan(&online(true), &settings(1e9)) else {
        panic!("压缩关着也要记 skipped");
    };
    assert_eq!(reason, "deferred_economic");
    assert_ne!(
        decision.archive_tokens, compaction_archive,
        "这是衰减的账：saving 不是压缩的 archive"
    );
}

/// 压缩关着、边界上也没有候选：没有东西可算，不记账。
#[test]
fn a_boundary_with_compaction_off_and_nothing_to_decay_is_left_alone() {
    let scenario = Scenario {
        compaction_enabled: false,
        ..Scenario::new(context())
    };
    assert!(matches!(
        scenario.plan(&online(true), &settings(1.25)),
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
        let Plan::Decay(job) = scenario.plan(&online(false), &settings(1.25)) else {
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
