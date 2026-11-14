//! 操作子命令的输出渲染（§3、§13.4）。
//!
//! 全是**纯函数**：协议类型进去，字符串出来。没有 I/O、没有时钟——「现在几点」由调用
//! 方传进来，否则定时清单的「还有多久」就没法测。
//!
//! 一条贯穿的规矩：**不知道就说不知道**。`??` 是 uncertain 的记号（§8.6：副作用是否
//! 发生未知，不能在这里假装失败）。

use komo_kernel::cron::{CronJob, FiringStatus, JobStatus, NotifyPolicy, OverlapPolicy, Trigger};
use komo_kernel::protocol::config::{ConfigIssue, IssueSeverity, SourceFile};
use komo_kernel::protocol::http::{
    CompactionSummary, ConfigCheckResponse, ConfigReloadResponse, ConsolidateAction,
    ConsolidateGroupStatus, ConsolidateOutcome, HealthResponse, IndexState, InterventionDetail,
    InterventionKind, InterventionListResponse, InterventionSummary, InterventionVerdict,
    MatterSchedule, McpReconnectResponse, McpServerState, McpServerStatus,
    MemoryConsolidateResponse, MemoryIndexStatus, MemoryListResponse, MemorySweepResponse,
    ModelWindow, MonitorListResponse, RunDetail, SessionListResponse, SessionSearchResponse,
    SessionSummary, SweepRule, TelemetryHealth, ToolCallSummary, WindowSource,
};
use komo_kernel::types::memory::{
    ArmScore, Confirmation, MemoryItem, MemoryKind, MemoryScope, MemoryState, Provenance,
    RecallDiagnostics, RetrievalMode,
};
use komo_kernel::types::status::{RetryCause, RunState, SessionState, ToolCallState, WaitReason};
use time::OffsetDateTime;

mod matter;
pub use matter::{matter_changed, matter_line, matter_list, matter_show};

/// 状态那一格：`Waiting` 时把**理由**接上（§8.4）。
///
/// 状态与理由是**两个维度**：状态说"能不能跑"，理由说"在等谁、等到什么时候"。清单与
/// `run inspect` 都要它，所以只写一处。
///
/// `Dependency` 只在这里与人见面：它**不进** §7.5 的清单（等前一条 Run 不是"等人"），
/// 于是"前面那条还没跑完"唯一说得清的地方就是这一格。
fn run_state_cell(state: RunState, wait: Option<&WaitReason>, now: OffsetDateTime) -> String {
    match (state, wait) {
        (RunState::Waiting, Some(reason)) => format!(
            "{} · {}",
            status_text(RunState::Waiting),
            wait_text(reason, Some(now))
        ),
        // `Waiting` 而没有理由：只说得出"停着"。**不编一个原因**——猜错比不说更糟。
        (state, _) => status_text(state).into(),
    }
}

/// `komo session list`。
///
/// 两列状态不是一回事：`运行` 是这个会话**当下那个 Run** 的状态（`Waiting` 时接上理由，
/// §8.4），`生命周期` 是会话自己在 §8.10 阶梯上的位置。`active` 不印（绝大多数会话都是
/// 它，印满一列噪音），非 `active` 必须印出来——一个正在关闭、已删除的会话与一个活会话
/// **在这一列唯一的区别**。
///
/// `now` 由调用方给（这个模块不读时钟）：等待理由里的"还有多久"要它才算得出来。
pub fn session_list(response: &SessionListResponse, now: OffsetDateTime) -> String {
    if response.sessions.is_empty() {
        return "没有会话".into();
    }
    let mut out = Vec::new();
    out.push(format!(
        "{:<38} {:<10} {:<44} {:<26} {}",
        "SESSION", "生命周期", "运行", "更新时间", "标题"
    ));
    for session in &response.sessions {
        out.push(format!(
            "{:<38} {:<10} {:<44} {:<26} {}",
            session.session,
            session_state(session.state),
            match session.current_state {
                Some(state) => run_state_cell(state, session.current_wait.as_ref(), now),
                None => "空闲".into(),
            },
            stamp(session.updated_at),
            session_title(session)
        ));
    }
    out.join("\n")
}

/// `komo session search`：每个命中的会话一段——会话 ID、种类与标题、最近命中的时刻与条数、
/// 能接着查的 ID（任务短号、派它的会话、还在跑的监听），再是片段。要看全文用 `komo session export`。
pub fn session_search(response: &SessionSearchResponse) -> String {
    if response.hits.is_empty() {
        return "没有匹配的会话".into();
    }
    let mut out = Vec::new();
    for hit in &response.hits {
        let title = if hit.title.is_empty() {
            "（无标题）"
        } else {
            hit.title.as_str()
        };
        out.push(format!("{} · {} · {title}", hit.session, hit.kind));
        let mut facts = vec![format!(
            "最近命中 {} · {} 处",
            stamp(hit.matched_at),
            hit.matches
        )];
        if let Some(task) = &hit.task {
            facts.push(format!("任务 {task}"));
        }
        if let Some(parent) = &hit.parent_session {
            facts.push(format!("派它的会话 {parent}"));
        }
        if !hit.monitors.is_empty() {
            facts.push(format!("监听 {}", hit.monitors.join("、")));
        }
        out.push(format!("  {}", facts.join(" · ")));
        for snippet in &hit.snippets {
            out.push(format!("  - {snippet}"));
        }
    }
    out.join("\n")
}

/// `komo monitor list`：一个监听一行——短号、状态、已跑多久、距截止还有多久、标题、挂它的会话；
/// 脚本缩进在下一行。`now` 由调用方给。
pub fn monitor_list(response: &MonitorListResponse, now: OffsetDateTime) -> String {
    if response.monitors.is_empty() {
        return "没有监听".into();
    }
    let mut out = Vec::new();
    for monitor in &response.monitors {
        let state = match (monitor.state.as_str(), &monitor.reason) {
            ("ended", Some(reason)) => format!("已结束（{reason}）"),
            ("ended", None) => "已结束".into(),
            ("starting", _) => "启动中".into(),
            _ => "运行中".into(),
        };
        let mut line = format!(
            "#{} {state} · 已跑 {}",
            monitor.handle,
            komo_kernel::types::task::duration_words(now - monitor.started_at)
        );
        if monitor.state != "ended" {
            line.push_str(&format!(
                " · 距截止 {}",
                komo_kernel::types::task::duration_words(monitor.deadline - now)
            ));
        }
        line.push_str(&format!(" · 「{}」", monitor.title));
        if let Some(session) = &monitor.session {
            line.push_str(&format!(" · 会话 {session}"));
        }
        out.push(line);
        out.push(format!("  {}", monitor.command));
    }
    out.join("\n")
}

/// §8.10 的四个状态在清单里的写法。`active` 是常态，留空。
fn session_state(state: SessionState) -> &'static str {
    match state {
        SessionState::Active => "",
        SessionState::Closing => "正在关闭",
        SessionState::Deleted => "已删除",
        SessionState::Purged => "已回收",
    }
}

fn session_title(session: &SessionSummary) -> String {
    let title = match (&session.title, &session.workdir) {
        (title, Some(workdir)) if title.is_empty() => format!("（无标题） · {workdir}"),
        (title, Some(workdir)) => format!("{title} · {workdir}"),
        (title, None) if title.is_empty() => "（无标题）".into(),
        (title, None) => title.clone(),
    };
    match task_marker(session) {
        Some(marker) => format!("{marker} {title}"),
        None => title,
    }
}

/// 任务会话的短标注（`docs/background-tasks.md`）：`任务·#k3x9qa`，
/// `#k3x9qa` 是这条会话自己的短号——Gateway 在 `SessionSummary.task` 里给，与派出回执、
/// `task_id` 同一个，操作者能直接拿它对上聊天里的那一条。老 Gateway 不给时退回会话 ID 后四位
/// （`komo_kernel::types::task::short_id`）。
///
/// 只有 `kind == "task"` 的会话才有这个标注；`main` / `normal` 一个字都不多印
/// （不改变现有输出，§ 约束）。
fn task_marker(session: &SessionSummary) -> Option<String> {
    if session.kind != "task" {
        return None;
    }
    let short = session
        .task
        .clone()
        .unwrap_or_else(|| komo_kernel::types::task::short_id(&session.session));
    Some(format!("任务·#{short}"))
}

/// `komo run inspect RUN_ID`。
///
/// `now` 由调用方给：`Waiting` 那一格的理由里"还有多久"要它才算得出来。
pub fn run_inspect(detail: &RunDetail, now: OffsetDateTime) -> String {
    let mut out = Vec::new();
    let summary = &detail.summary;
    out.push(format!("Run    {}", summary.run));
    out.push(format!("会话   {}", summary.session));
    out.push(format!(
        "状态   {}{}",
        run_state_cell(summary.state, summary.wait.as_ref(), now),
        match (summary.state, summary.wait.as_ref()) {
            // 理由不在（老数据、或者服务端没给）时至少说清"停着不是结束"；理由在的时候
            // 这句话是废话——它已经说清在等谁了。
            (RunState::Waiting, None) => "（停着，不是结束）",
            (RunState::Abandoned, _) => "（操作者在 Intervention 清单上放弃，不等于用户取消）",
            _ => "",
        }
    ));
    out.push(format!("轮数   {}", summary.rounds));
    if let Some(model) = &detail.model {
        out.push(format!("模型   {model}"));
    }
    out.push(format!(
        "开始   {}{}{}",
        stamp(summary.created_at),
        match summary.ended_at {
            Some(ended) => format!("   结束 {}", stamp(ended)),
            None => String::new(),
        },
        match detail.duration_ms {
            Some(ms) if ms < 1000 => format!("   耗时 {ms}ms"),
            Some(ms) => format!("   耗时 {}", human_span((ms / 1000) as i64)),
            None => String::new(),
        }
    ));
    if let Some(reason) = detail.failure.as_deref().filter(|r| !r.trim().is_empty()) {
        out.push(format!("失败   {}", one_line(reason)));
    }
    if let Some(tokens) = &detail.tokens {
        let cells: Vec<String> = [
            ("输入", tokens.input),
            ("输出", tokens.output),
            ("缓存读", tokens.cache_read),
            ("缓存写", tokens.cache_write),
        ]
        .into_iter()
        .filter_map(|(name, value)| value.map(|value| format!("{name} {value}")))
        .collect();
        out.push(format!("用量   {}", cells.join(" · ")));
    }
    for compaction in &detail.compactions {
        out.push(format!("压缩   {}", compaction_line(compaction)));
    }

    if detail.calls.is_empty() {
        out.push("调用   （无）".into());
    } else {
        out.push("调用".into());
        for call in &detail.calls {
            out.push(format!("  {}", call_line(call)));
            if let Some(preview) = call.preview.as_deref().filter(|p| !p.trim().is_empty()) {
                for line in preview.lines().take(3) {
                    out.push(format!("      │ {line}"));
                }
            }
            if let Some(output) = &call.output {
                out.push(format!("      → {}", output.path()));
            }
        }
    }

    if !detail.memories.is_empty() {
        // §9.7：这次 Run 用到了哪些记忆，是审计证据。
        let used: Vec<String> = detail
            .memories
            .iter()
            .map(|use_| format!("{}@{}", use_.memory, use_.revision))
            .collect();
        out.push(format!("记忆   {}", used.join(" · ")));
    }
    if let Some(recall) = &detail.recall {
        out.push(format!("召回   {}", recall_line(recall)));
    }

    if let Some(message) = &detail.final_message {
        out.push("最终回复".into());
        for line in message.lines() {
            out.push(format!("  {line}"));
        }
    }
    out.join("\n")
}

fn compaction_line(compaction: &CompactionSummary) -> String {
    let mut cells = vec![match compaction.outcome.as_str() {
        "compacted" => "摘要".to_string(),
        "decayed" => match compaction.decayed_calls {
            Some(calls) => format!("衰减 {calls} 个结果"),
            None => "衰减".to_string(),
        },
        "skipped" => "未压".to_string(),
        other => other.to_string(),
    }];
    if let Some(archive) = compaction.archive_tokens {
        cells.push(format!("原 {archive} tok"));
    }
    if let Some(post) = compaction.post_compaction_tokens {
        cells.push(format!("压后上下文 {post} tok"));
    }
    if let Some(reason) = &compaction.reason {
        cells.push(reason.clone());
    }
    cells.join(" · ")
}

fn recall_line(recall: &RecallDiagnostics) -> String {
    let mut cells = Vec::new();
    if recall.reused_pin {
        cells.push("沿用这一段钉下的那块".to_string());
    }
    if let Some(mode) = recall.mode {
        cells.push(
            match mode {
                RetrievalMode::Hybrid => "hybrid",
                RetrievalMode::Keyword => "keyword",
                RetrievalMode::Vector => "vector",
            }
            .to_string(),
        );
    }
    if recall.degraded {
        cells.push(match recall.degraded_reason.as_deref() {
            Some(reason) => format!("降级（{}）", one_line(reason)),
            None => "降级".to_string(),
        });
    }
    if let (Some(before), Some(after)) = (
        recall.candidates_before_fusion,
        recall.candidates_after_fusion,
    ) {
        cells.push(format!("候选 {before}→{after}"));
    }
    if let Some(dropped) = recall.dropped_by_budget.filter(|dropped| *dropped > 0) {
        cells.push(format!("预算挤掉 {dropped}"));
    }
    if let Some(over) = recall.conventions_over_budget.filter(|over| *over > 0) {
        cells.push(format!("约定超出预算 {over} 条"));
    }
    if let Some(over) = recall.matters_over_budget.filter(|over| *over > 0) {
        cells.push(format!("事项超出预算 {over} 条"));
    }
    cells.join(" · ")
}

fn call_line(call: &ToolCallSummary) -> String {
    let mut line = format!(
        "{} {:<10} {}",
        call_marker(call.state),
        call.tool,
        call.call
    );
    if call.attempts > 1 {
        line.push_str(&format!("  尝试 {} 次", call.attempts));
    }
    if call.state == ToolCallState::Uncertain {
        // §8.6：不能用「重试成功」盖掉这段窗口。
        line.push_str("  结果不明——先核对目标状态再决定是否重试");
    }
    line
}

/// **`??` 是 uncertain。**「那件事做了没有」有三个答案，不是两个。
pub fn call_marker(state: ToolCallState) -> &'static str {
    match state {
        ToolCallState::Planned => "··",
        ToolCallState::Started => "▶ ",
        ToolCallState::Completed => "ok",
        ToolCallState::Failed => "!!",
        ToolCallState::Uncertain => "??",
    }
}

/// `komo intervention list`：结果不明、阻塞两类一张表（§7.5）。
///
/// 每一行都带**句柄**（Run ID）、**种类**、**问题**与**可答的结论**
/// ——§11.3 要求界面「写清这一条该答什么」。结论直接印线格式的词（`satisfied` /
/// `not_performed` / `resolve` / `abandon`），因为答的时候打的就是它。
pub fn interventions(response: &InterventionListResponse) -> String {
    if response.interventions.is_empty() {
        return "没有待处理的事项".into();
    }
    let mut out = vec![format!(
        "{:<38} {:<10} {:<38} {:<44} {}",
        "句柄", "种类", "会话", "问题", "可答结论"
    )];
    for item in &response.interventions {
        out.push(format!(
            "{:<38} {:<10} {:<38} {:<44} {}",
            item.handle,
            intervention_kind_text(item.kind),
            item.session,
            one_line(&item.question),
            verdicts_text(&item.verdicts)
        ));
    }
    out.push(String::new());
    out.push("`komo intervention answer <句柄> <结论>`，一次答一条".into());
    out.join("\n")
}

/// `komo intervention show <句柄>`：单项详情，两类各印各的。
///
/// `verify` 多一次调用的名字与计划哈希——核对"到底发生没有"看的就是它；`blocked` 印的是
/// 前提为什么没了。
pub fn intervention_detail(detail: &InterventionDetail) -> String {
    match detail {
        InterventionDetail::Verify {
            summary,
            tool,
            plan_hash,
            reason,
        } => {
            let mut out = intervention_head(summary);
            out.push(format!("调用   {tool}"));
            if let Some(hash) = plan_hash {
                out.push(format!("计划   {}", hash.as_str()));
            }
            out.push(String::new());
            out.push(format!("问题   {}", reason));
            out.join("\n")
        }
        InterventionDetail::Blocked { summary, reason } => {
            let mut out = intervention_head(summary);
            out.push(String::new());
            out.push(format!("问题   {reason}"));
            out.join("\n")
        }
        InterventionDetail::Confirm {
            summary,
            tool,
            arguments,
            deadline,
        } => {
            let mut out = intervention_head(summary);
            out.push(format!("调用   {tool}"));
            out.push(format!("参数   {arguments}"));
            out.push(format!(
                "截止   {}（到点没答就不执行）",
                deadline
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default()
            ));
            out.join("\n")
        }
    }
}

/// 两类的公共抬头：句柄、种类、会话、Run、问题、可答结论。
fn intervention_head(summary: &InterventionSummary) -> Vec<String> {
    let mut out = vec![
        format!(
            "句柄   {}（{}）",
            summary.handle,
            intervention_kind_text(summary.kind)
        ),
        format!("会话   {}", summary.session),
    ];
    out.push(format!(
        "Run    {}",
        summary
            .run
            .as_ref()
            .map(|run| run.to_string())
            .unwrap_or_else(|| "—".into())
    ));
    out.push(format!("问题   {}", summary.question));
    out.push(format!("可答   {}", verdicts_text(&summary.verdicts)));
    out
}

/// 带定时的目标的定时清单（表头 + 每个目标两三行）。
///
/// 每一行是"这个定时是什么"，下一行是"它上一次怎么样了"——§10 的
/// 「Cron 的结果去原 Session 查看」需要先知道有没有结果、是哪一个 Session。
pub fn schedule_list(schedules: &[MatterSchedule], now: OffsetDateTime) -> String {
    if schedules.is_empty() {
        return "没有定时".into();
    }
    let mut out = vec![format!(
        "{:<38} {:<10} {:<8} {:<8} {:<36} {}",
        "目标", "状态", "重叠", "投递", "下次", "标题 / 触发"
    )];
    for schedule in schedules {
        out.extend(schedule_lines(schedule, now));
    }
    out.join("\n")
}

/// 一个目标的定时：第一行是配置与下一次，往下是命令、上一次触发、触发层面的问题。
pub fn schedule_lines(schedule: &MatterSchedule, now: OffsetDateTime) -> Vec<String> {
    let job = &schedule.job;
    let mut out = vec![format!(
        "{:<38} {:<10} {:<8} {:<8} {:<36} {} · {}",
        job.id,
        job_status(job.status),
        overlap(job.overlap),
        notify(job.notify),
        match job.next_run_at {
            Some(next) => format!("{} ({})", stamp(next), relative(next, now)),
            None => "—".into(),
        },
        job.name,
        trigger(&job.trigger)
    )];
    // 命令直跑模式（§10）：这是判断"这个定时会不会经过模型"的第一眼，命令本身
    // 也要看得见——过长截断，不然一行把整张表拉宽。
    if let Some(command) = &job.command {
        out.push(format!("  [命令] {}", truncate(command, 80)));
    }
    if let Some(last) = &schedule.last {
        // 「更新本次触发状态」（§10）：ok / error / waiting / skipped 都要看得见，
        // 否则一个天天被跳过的定时和一个天天跑成的长得一样。
        let mut line = format!(
            "  上次 {} · {}",
            stamp(last.scheduled_at),
            firing_status(last.status)
        );
        if let Some(session) = &last.session {
            line.push_str(&format!(" · 会话 {session}"));
        }
        if let Some(error) = &last.error {
            line.push_str(&format!(" · {error}"));
        }
        out.push(line);
    }
    if let Some(error) = &job.last_error {
        // 一个再也不响的定时应当在清单里看得见。
        out.push(format!("  ⚠ {error}"));
    }
    out
}

/// 目标清单行尾的一小段：有定时就说一句节奏、启停与下一次。
pub fn schedule_brief(job: &CronJob) -> String {
    let next = match job.next_run_at {
        Some(next) => stamp(next),
        None => "—".into(),
    };
    format!(
        "定时 {} · {} · 下次 {next}",
        trigger(&job.trigger),
        job_status(job.status)
    )
}

/// 过长截断，按字符数（不是字节数）算，免得在多字节字符中间切断。
fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn notify(policy: NotifyPolicy) -> &'static str {
    match policy {
        NotifyPolicy::Always => "总是",
        NotifyPolicy::OnError => "仅出错",
        NotifyPolicy::Never => "不投",
    }
}

fn firing_status(status: FiringStatus) -> &'static str {
    match status {
        FiringStatus::Queued => "排队中",
        FiringStatus::Ok => "ok",
        FiringStatus::Error => "error",
        FiringStatus::Waiting => "waiting（在等人）",
        FiringStatus::Skipped => "skipped",
    }
}

fn job_status(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Active => "启用",
        JobStatus::Paused => "暂停",
        JobStatus::Done => "已完成",
    }
}

fn overlap(policy: OverlapPolicy) -> &'static str {
    match policy {
        OverlapPolicy::Skip => "跳过",
        OverlapPolicy::Allow => "并行",
    }
}

fn trigger(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Cron { expr, tz } => format!("{expr} @{}", tz.name()),
        Trigger::At { at } => format!("@at {}", stamp(*at)),
    }
}

/// `komo memory list`。**来源与确认状态都印出来**（§9.2：两者分开保存）。
pub fn memory_list(response: &MemoryListResponse) -> String {
    let mut out = Vec::new();
    if response.degraded {
        // §9.4：检索降级要明说。
        out.push(format!(
            "⚠ 检索已降级为关键词{}",
            response
                .degraded_reason
                .as_deref()
                .map(|r| format!("：{r}"))
                .unwrap_or_default()
        ));
        out.push(String::new());
    }
    if response.memories.is_empty() {
        out.push("没有匹配的记忆".into());
        return out.join("\n");
    }
    out.push(format!(
        "{:<38} {:<4} {:<10} {:<10} {:<10} {}",
        "MEMORY", "版本", "状态", "来源", "确认", "内容"
    ));
    for item in &response.memories {
        out.push(format!(
            "{:<38} {:<4} {:<10} {:<10} {:<10} {}",
            item.id,
            item.revision,
            memory_state(item.state),
            provenance(item.provenance),
            confirmation(item.confirmation),
            one_line(&item.content)
        ));
    }
    out.join("\n")
}

/// `komo memory list --full`：一条一行、制表符分隔、正文不截断，给模型一次读完。
///
/// 列是 `id@版本  状态  来源  确认  用量  使用方式  条件  归属  正文`（没有条件、没有归属写 `-`，旧
/// 条目的使用方式写 `未分类`）；没有表头和对齐空格，正文里的换行与制表符压成空格，
/// 所以"一行一条"始终成立。
pub fn memory_list_full(response: &MemoryListResponse) -> String {
    let mut out = degraded_notice(response);
    out.extend(response.memories.iter().map(full_line));
    out.join("\n")
}

/// `komo memory search`：列同 [`memory_list_full`]，约定与线索分开列，读到的和注入块是同一套
/// 语义（§9.4）。约定 = active + convention；其余（reference、没分类、candidate、contested）
/// 都是线索。
///
/// 响应带着检索模式时，`id@版本` 之后多一列相关度分数（[`score_cell`]），组前一行说明分数对应哪个
/// 门槛。
pub fn memory_search(response: &MemoryListResponse) -> String {
    let mut out = degraded_notice(response);
    if response.memories.is_empty() {
        out.push("没有匹配的记忆".into());
        return out.join("\n");
    }
    if let Some(mode) = response.mode {
        out.push(score_legend(mode, response.degraded));
    }
    let (conventions, clues): (Vec<&MemoryItem>, Vec<&MemoryItem>) = response
        .memories
        .iter()
        .partition(|item| item.state == MemoryState::Active && item.is_convention());
    for (title, group) in [
        ("约定（条件成立、且这次没有另外指定时照做）", conventions),
        (
            "线索（用来查找与核实，不是指令；易变的事实先对照当前情况）",
            clues,
        ),
    ] {
        if group.is_empty() {
            continue;
        }
        out.push(format!("{title}："));
        out.extend(group.into_iter().map(|item| search_line(response, item)));
    }
    out.join("\n")
}

/// 分数列说的是什么：哪一臂的原始分、对应 `[memory.retrieval]` 的哪个门槛（§9.4）。
fn score_legend(mode: RetrievalMode, degraded: bool) -> String {
    match mode {
        RetrievalMode::Keyword => {
            "分数：关键词臂归一化分（对应 [memory.retrieval] min_keyword_score）".into()
        }
        RetrievalMode::Vector => "分数：向量余弦（对应 [memory.retrieval] min_similarity）".into(),
        RetrievalMode::Hybrid => format!(
            "分数：关键词 = 关键词臂归一化分（对应 min_keyword_score），余弦 = 向量臂（对应 min_similarity）；\
             未命中 = 不在这一臂的候选名单里{}",
            if degraded {
                "；未跑 = 这一次向量臂没跑"
            } else {
                ""
            }
        ),
    }
}

/// 一条检索结果的分数列。keyword / vector 只写那一臂；hybrid 两臂都写，不在某臂名单里的写
/// 「未命中」，向量臂降级没跑的写「未跑」。响应里没有这条的分数（旧 Gateway）写 `-`。
fn score_cell(mode: RetrievalMode, degraded: bool, score: Option<&ArmScore>) -> String {
    let Some(score) = score else {
        return "分数 -".into();
    };
    let value = |score: Option<f32>| score.map_or_else(|| "未命中".into(), |s| format!("{s:.3}"));
    let keyword = format!("关键词 {}", value(score.keyword));
    let vector = if degraded {
        "余弦 未跑".to_string()
    } else {
        format!("余弦 {}", value(score.vector))
    };
    match mode {
        RetrievalMode::Keyword => keyword,
        RetrievalMode::Vector => vector,
        RetrievalMode::Hybrid => format!("{keyword} · {vector}"),
    }
}

fn search_line(response: &MemoryListResponse, item: &MemoryItem) -> String {
    let Some(mode) = response.mode else {
        return full_line(item);
    };
    let score = response.arm_scores.iter().find(|score| score.id == item.id);
    format!(
        "{}@{}\t{}\t{}",
        item.id,
        item.revision,
        score_cell(mode, response.degraded, score),
        full_fields(item)
    )
}

fn degraded_notice(response: &MemoryListResponse) -> Vec<String> {
    if !response.degraded {
        return Vec::new();
    }
    vec![format!(
        "⚠ 检索已降级为关键词{}",
        response
            .degraded_reason
            .as_deref()
            .map(|r| format!("：{r}"))
            .unwrap_or_default()
    )]
}

fn full_line(item: &MemoryItem) -> String {
    format!("{}@{}\t{}", item.id, item.revision, full_fields(item))
}

/// `id@版本` 之后的那些列。
fn full_fields(item: &MemoryItem) -> String {
    let content = item
        .content
        .split(['\n', '\r', '\t'])
        .filter(|part| !part.trim().is_empty())
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{}\t{}\t{}\t注入 {} 次 · 用上 {} 次\t{}\t{}\t{}\t{}",
        memory_state(item.state),
        provenance(item.provenance),
        confirmation(item.confirmation),
        item.usage.count,
        item.usage.adopted_count,
        application_text(item),
        item.applies_when.as_deref().unwrap_or("-"),
        item.belongs_to
            .as_ref()
            .map_or_else(|| "-".to_string(), ToString::to_string),
        content
    )
}

/// `komo memory add`：先一句说清记成了什么，再是整条。
pub fn memory_added(response: &komo_kernel::protocol::http::MemoryAddResponse) -> String {
    let item = &response.memory;
    let head = match (response.created, item.confirmation) {
        (true, Confirmation::UserConfirmed) => "已记下（已确认）".to_string(),
        (true, Confirmation::Unconfirmed) => format!(
            "已记下（未确认）：要确认就跑 `komo memory confirm {} --revision {}`",
            item.id, item.revision
        ),
        (false, _) => "库里已有逐字相同的一条：只补了证据 / 确认".to_string(),
    };
    format!("{head}\n\n{}", memory_show(item))
}

/// 使用方式的显示：没分类的旧条目如实写"未分类"（按线索用，§9.2）。
fn application_text(item: &MemoryItem) -> &'static str {
    match item.application {
        Some(application) => application.as_str(),
        None => "未分类",
    }
}

/// `komo memory show <id>`。
pub fn memory_show(item: &MemoryItem) -> String {
    let mut out = vec![
        format!("MEMORY  {}  (revision {})", item.id, item.revision),
        format!("内容    {}", item.content),
        format!(
            "分类    {} · {}",
            memory_kind(item.kind),
            scope_text(&item.scope)
        ),
        format!(
            "来源    {}   确认    {}",
            provenance(item.provenance),
            confirmation(item.confirmation)
        ),
        format!("状态    {}", memory_state(item.state)),
        format!(
            "使用方式  {}   条件  {}",
            application_text(item),
            item.applies_when.as_deref().unwrap_or("（无）")
        ),
        // 观察时间与入库时间是两回事（§9.2）。
        format!(
            "观察    {}   入库 {}   更新 {}",
            stamp(item.observed_at),
            stamp(item.created_at),
            stamp(item.updated_at)
        ),
    ];
    if let Some(matter) = &item.belongs_to {
        out.push(format!("归属    {matter}"));
    }
    if let Some(until) = item.valid_until {
        out.push(format!("有效期  至 {}", stamp(until)));
    }
    // 取代关系两个方向都显示：从任一方都能找到另一方（§9.6、§9.8）。
    if let Some(superseded) = &item.supersedes {
        out.push(format!(
            "取代了    {}@{}",
            superseded.memory, superseded.revision
        ));
    }
    if let Some(by) = &item.superseded_by {
        out.push(format!("被取代    {}@{}", by.memory, by.revision));
    }
    out.push(format!(
        "提取    {} / {} / prompt {}",
        item.extraction.model,
        item.extraction
            .effort
            .as_option()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "服务端默认".into()),
        item.extraction.prompt_version
    ));
    out.push(format!(
        "使用    注入 {} 次{} · 用上 {} 次{}",
        item.usage.count,
        item.usage
            .last_used_at
            .map(|at| format!("（最近 {}）", stamp(at)))
            .unwrap_or_default(),
        item.usage.adopted_count,
        item.usage
            .last_adopted_at
            .map(|at| format!("（最近 {}）", stamp(at)))
            .unwrap_or_default()
    ));
    if item.evidence.is_empty() {
        out.push("证据    （无）".into());
    } else {
        out.push("证据".into());
        for evidence in &item.evidence {
            out.push(format!(
                "  {} · {} · {}",
                evidence_ref(&evidence.reference),
                provenance(evidence.provenance),
                stamp(evidence.observed_at)
            ));
        }
    }
    out.join("\n")
}

fn evidence_ref(reference: &komo_kernel::types::memory::EvidenceRef) -> String {
    match reference {
        komo_kernel::types::memory::EvidenceRef::Event {
            session,
            event,
            seq,
        } => format!("{session}#{seq}（{event}）"),
        komo_kernel::types::memory::EvidenceRef::Memos {
            instance,
            record_id,
            ..
        } => format!("memos {instance}/{record_id}"),
    }
}

fn memory_state(state: MemoryState) -> &'static str {
    match state {
        MemoryState::Candidate => "候选",
        MemoryState::Active => "生效",
        MemoryState::Contested => "有冲突",
        MemoryState::Superseded => "被取代",
        MemoryState::Forgotten => "已停用",
        MemoryState::Archived => "已归档",
    }
}

fn memory_kind(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Preference => "偏好",
        MemoryKind::Fact => "事实",
        MemoryKind::Experience => "经验",
    }
}

fn provenance(provenance: Provenance) -> &'static str {
    match provenance {
        Provenance::UserStatement => "用户原话",
        Provenance::ToolObservation => "工具观察",
        Provenance::ModelInference => "模型推断",
    }
}

fn confirmation(confirmation: Confirmation) -> &'static str {
    match confirmation {
        Confirmation::Unconfirmed => "未确认",
        Confirmation::UserConfirmed => "已确认",
    }
}

fn scope_text(scope: &MemoryScope) -> String {
    match scope {
        MemoryScope::Personal => "个人".into(),
        MemoryScope::Project { project_id } => format!("项目 {project_id}"),
        MemoryScope::Environment { instance_id } => format!("环境 {instance_id}"),
        MemoryScope::Agent { agent_id } => format!("助手 {agent_id}"),
    }
}

/// `komo memory consolidate`（§9.8）。预演与执行同一种清单：每组成员 `id@revision`、
/// 模型给的动作与理由、授权结果。动作里的 id 都带上组里记下的 revision，方便照抄。
pub fn memory_consolidate(response: &MemoryConsolidateResponse) -> String {
    let mut out = vec![format!(
        "{} {} 组；成员没变而跳过 {} 组；留给下次 {} 组",
        if response.applied {
            "已整理"
        } else {
            "预演，将整理"
        },
        response.groups.len(),
        response.unchanged_groups,
        response.deferred_groups
    )];
    for (index, group) in response.groups.iter().enumerate() {
        let at = |id: &komo_kernel::types::ids::MemoryId| match group
            .members
            .iter()
            .find(|member| &member.memory == id)
        {
            Some(member) => format!("{}@{}", member.memory, member.revision),
            None => id.to_string(),
        };
        let action_text = |action: &ConsolidateAction| match action {
            ConsolidateAction::Merge { keep, absorb } => format!(
                "合并 保留 {}，并入 {}",
                at(keep),
                absorb.iter().map(&at).collect::<Vec<_>>().join("、")
            ),
            ConsolidateAction::Supersede { old, by } => {
                format!("取代 {} 被 {} 取代", at(old), at(by))
            }
            ConsolidateAction::Contest { a, b } => format!("冲突 {} 与 {}", at(a), at(b)),
            ConsolidateAction::Rereadable { item } => format!("可重读 {}", at(item)),
        };
        out.push(format!(
            "\n组 {}（{}，{} 条）{}",
            index + 1,
            scope_text(&group.scope),
            group.members.len(),
            match group.status {
                ConsolidateGroupStatus::Judged if group.actions.is_empty() => "：保持原样",
                ConsolidateGroupStatus::Judged => "",
                ConsolidateGroupStatus::Conflict => "：判定之后有条目变了，整组跳过，没盖游标",
                ConsolidateGroupStatus::Failed => "：失败，没盖游标",
            }
        ));
        for member in &group.members {
            out.push(format!("  · {}@{}", member.memory, member.revision));
        }
        for entry in &group.actions {
            let outcome = match entry.outcome {
                ConsolidateOutcome::Executed => "执行",
                ConsolidateOutcome::Downgraded => "降级",
                ConsolidateOutcome::Skipped => "跳过",
            };
            out.push(format!(
                "  [{outcome}] {}｜{}",
                action_text(&entry.action),
                entry.reason
            ));
            if let Some(executed) = &entry.executed_as {
                out.push(format!("      实际：{}", action_text(executed)));
            }
            if let Some(note) = &entry.note {
                out.push(format!("      {note}"));
            }
        }
        if let Some(detail) = &group.detail {
            out.push(format!("  ✗ {detail}"));
        }
    }
    if !response.applied && !response.groups.is_empty() {
        out.push("\n预演不写库；加 --apply 会重新判定后执行（不执行这份清单）".into());
    }
    out.join("\n")
}

/// `komo memory sweep`。每条都带 id@revision 与理由，撤回时照着抄 `komo memory restore`。
pub fn memory_sweep(response: &MemorySweepResponse) -> String {
    let verb = if response.applied {
        "已处理"
    } else {
        "预演，将处理"
    };
    let mut out = vec![format!(
        "看过 {} 条，{} {} 条",
        response.scanned,
        verb,
        response.actions.len()
    )];
    for action in &response.actions {
        out.push(format!(
            "{}@{}  {} → {}  {}｜{}",
            action.memory,
            action.revision,
            match action.rule {
                SweepRule::Expired => "过期",
                SweepRule::NeverUsed => "从没用过",
                SweepRule::Idle => "久未使用",
                SweepRule::MissingPath => "路径失效",
            },
            memory_state(action.to),
            action.detail,
            one_line(&action.content)
        ));
    }
    for error in &response.errors {
        out.push(format!("✗ {error}"));
    }
    if !response.applied && !response.actions.is_empty() {
        out.push("加 --apply 才会真的执行".into());
    }
    out.join("\n")
}

/// `komo memory index status`。
pub fn memory_index(status: &MemoryIndexStatus) -> String {
    let mut out = vec![format!(
        "索引状态  {}",
        match status.state {
            IndexState::Unconfigured => "未配置向量模型（只有关键词臂）",
            IndexState::Building => "构建中",
            IndexState::Ready => "就绪",
            IndexState::Failed => "失败",
        }
    )];
    if let Some(space) = &status.space {
        out.push(format!(
            "向量空间  {} / {} / {} 维{}",
            space.provider,
            space.model,
            space.dimensions,
            space
                .revision
                .as_deref()
                .map(|r| format!(" / rev {r}"))
                .unwrap_or_default()
        ));
        out.push(format!("指纹      {}", space.fingerprint().as_str()));
    }
    if let Some(generation) = &status.generation {
        out.push(format!("代次      {generation}"));
    }
    out.push(format!(
        "覆盖率    {:.1}%（{} / {}）",
        status.coverage * 100.0,
        status.indexed,
        status.total
    ));
    for error in &status.errors {
        out.push(format!("⚠ {error}"));
    }
    out.join("\n")
}

/// `komo config check`：**每条问题定位到键**（§3 第 1 步）。
///
/// 只读，不改变运行中的 Gateway——所以它顺带把「当前生效的那份是什么时候装的、来源
/// 文件什么时候改的」也印出来，那是同一个问题的另一半。
pub fn config_check(response: &ConfigCheckResponse) -> String {
    let mut out = vec![issues_text(&response.issues)];
    out.push(String::new());
    out.push(format!("当前配置装载于 {}", stamp(response.loaded_at)));
    out.extend(source_lines(response.loaded_at, &response.sources));
    if let Some(warning) = stale_warning(response.loaded_at, &response.sources) {
        out.push(warning);
    }
    out.extend(window_lines(&response.model_windows));
    out.join("\n")
}

/// 各 completion alias 的上下文窗口；未知的提示去配置里写。
fn window_lines(windows: &[ModelWindow]) -> Vec<String> {
    if windows.is_empty() {
        return Vec::new();
    }
    let mut out = vec![String::new(), "上下文窗口".into()];
    for item in windows {
        let line = match (item.source, item.window) {
            (WindowSource::Config, Some(n)) => format!("{n}（配置）"),
            (WindowSource::Provider, Some(n)) => format!("{n}（provider）"),
            _ => format!(
                "未知（{}）",
                item.detail.as_deref().unwrap_or("provider 没有报告")
            ),
        };
        out.push(format!("  {:<24} {line}", item.alias));
    }
    if windows.iter().any(|w| w.window.is_none()) {
        out.push("  未知的窗口可在 model.<alias>.context_window 里写明".into());
    }
    out
}

/// `komo mcp reconnect`：重连等握手，每台报连完之后的状态（`docs/mcp.md` §2.5）。
pub fn mcp_reconnect(response: &McpReconnectResponse) -> String {
    if response.servers.is_empty() {
        return "config.toml 里没有配置 MCP 服务器".into();
    }
    let mut out = vec!["MCP 服务器已重连：".to_string()];
    out.extend(mcp_server_lines(&response.servers));
    if response
        .servers
        .iter()
        .any(|server| server.state == McpServerState::Connected)
    {
        out.push(String::new());
        out.push("新连上的工具从下一条 Run 起可用。".into());
    }
    out.join("\n")
}

/// 每台 MCP 服务器一行：名字、状态，有原因就跟在后面。
fn mcp_server_lines(servers: &[McpServerStatus]) -> Vec<String> {
    servers
        .iter()
        .map(|server| {
            let state = match server.state {
                McpServerState::Connecting => "连接中",
                McpServerState::Connected => "已连上",
                McpServerState::NeedsAuth => "要登录",
                McpServerState::Failed => "失败",
                McpServerState::Disconnected => "已断开",
                McpServerState::Disabled => "已关闭",
            };
            match &server.detail {
                Some(detail) => format!("  {:<12} {state}  {detail}", server.name),
                None => format!("  {:<12} {state}", server.name),
            }
        })
        .collect()
}

/// `komo config reload`：**成功**的那一支。
///
/// 校验不过根本走不到这里——那是一个带 `keys` 的 [`ErrorCode::ConfigInvalid`]
/// （`render_config_error`），旧快照原样保留。
pub fn config_reload(response: &ConfigReloadResponse) -> String {
    let mut out = Vec::new();
    if response.changed.is_empty() {
        out.push("配置已重新装载，没有键变化".into());
    } else {
        out.push(format!(
            "配置已重新装载，{} 个键变化",
            response.changed.len()
        ));
        for key in &response.changed {
            out.push(format!("  {key}"));
        }
    }
    if !response.start_only.is_empty() {
        // §3 第 4 步：不静默忽略，也不假装已生效。
        out.push(String::new());
        out.push("以下键**只在启动时生效**，这次没有生效，需要 `komo gateway restart`：".into());
        for key in &response.start_only {
            out.push(format!("  {key}"));
        }
    }
    if !response.mcp.is_empty() {
        // 重载不等握手（`docs/mcp.md` §2.2）：刚加的多半还在连，连上后从下一条 Run 起可用。
        out.push(String::new());
        out.push("MCP 服务器：".into());
        out.extend(mcp_server_lines(&response.mcp));
    }
    if !response.warnings.is_empty() {
        out.push(String::new());
        for issue in &response.warnings {
            out.push(format!("警告  {}: {}", issue.key, issue.message));
        }
    }
    out.join("\n")
}

/// `komo config reload` 的**失败**那一支：把 [`ErrorCode::ConfigInvalid`] 的 `keys`
/// 印成人能直接去改的样子。
pub fn config_error(error: &crate::error::ClientError) -> String {
    let mut out = vec![error.to_string()];
    for key in error.keys() {
        out.push(format!("  {key}"));
    }
    if error.is(komo_kernel::protocol::http::ErrorCode::ConfigInvalid) {
        // §3 第 1 步：校验不过的配置永远不会被装上，哪怕只错一个键。
        out.push("运行中的 Gateway 保留原配置".into());
    }
    out.join("\n")
}

/// `komo doctor`。
///
/// §3：「显示当前生效配置的加载时间与来源文件 mtime，两者不一致就是『文件改了但没装
/// 上』，把上一次校验错误一并印出来。」——两件事都在 [`ConfigCheckResponse`] 里，所以
/// doctor 就是 health 加它。
pub fn doctor(health: &HealthResponse, config: Option<&ConfigCheckResponse>) -> String {
    let mut out = vec![
        format!("实例      {}", health.instance_id),
        format!(
            "版本      {}（协议 v{}）",
            health.version, health.protocol_version
        ),
        format!("启动于    {}", stamp(health.started_at)),
        format!("数据目录  {}", health.data_dir),
        telemetry_line(&health.telemetry),
    ];

    match config {
        None => out.push("配置      取不到当前快照".into()),
        Some(config) => {
            out.push(format!("配置装载  {}", stamp(config.loaded_at)));
            out.extend(source_lines(config.loaded_at, &config.sources));
            if let Some(warning) = stale_warning(config.loaded_at, &config.sources) {
                out.push(warning);
            }
            if !config.issues.is_empty() {
                out.push(String::new());
                out.push("上一次校验".into());
                out.push(issues_text(&config.issues));
            }
        }
    }
    out.join("\n")
}

fn telemetry_line(telemetry: &TelemetryHealth) -> String {
    let Some(endpoint) = &telemetry.endpoint else {
        return "遥测      未配置".into();
    };
    let last = match (telemetry.last_ok_at, telemetry.last_error_at) {
        (None, None) => "还没导出过".to_string(),
        (ok, Some(error)) if ok.is_none_or(|ok| error >= ok) => format!(
            "最近导出失败 {}：{}",
            stamp(error),
            telemetry.last_error.as_deref().unwrap_or("未知原因")
        ),
        (Some(ok), _) => format!("最近导出成功 {}", stamp(ok)),
        (None, Some(_)) => unreachable!("上一个分支吃掉了 ok 为空的情形"),
    };
    format!("遥测      {endpoint}（{last}）")
}

fn issues_text(issues: &[ConfigIssue]) -> String {
    if issues.is_empty() {
        return "配置校验通过".into();
    }
    let errors = issues
        .iter()
        .filter(|issue| issue.severity == IssueSeverity::Error)
        .count();
    let mut out = Vec::new();
    for issue in issues {
        let tag = match issue.severity {
            IssueSeverity::Error => "错误",
            IssueSeverity::Warning => "警告",
        };
        out.push(format!("{tag}  {}: {}", issue.key, issue.message));
    }
    out.push(String::new());
    out.push(if errors > 0 {
        // §3：校验不过的配置永远不会被装上，哪怕只错一个键。
        format!("{errors} 个错误——这份配置不会被装上，运行中的 Gateway 保留原配置")
    } else {
        format!("{} 个警告，没有错误", issues.len())
    });
    out.join("\n")
}

fn source_lines(loaded_at: OffsetDateTime, sources: &[SourceFile]) -> Vec<String> {
    sources
        .iter()
        .map(|source| {
            format!(
                "  {} {}   mtime {}",
                if source.mtime > loaded_at { "⚠" } else { " " },
                source.path.display(),
                stamp(source.mtime)
            )
        })
        .collect()
}

/// 文件比生效的那份新 = 「文件改了但没装上」（§3）。
fn stale_warning(loaded_at: OffsetDateTime, sources: &[SourceFile]) -> Option<String> {
    let stale: Vec<String> = sources
        .iter()
        .filter(|source| source.mtime > loaded_at)
        .map(|source| source.path.display().to_string())
        .collect();
    (!stale.is_empty()).then(|| {
        format!(
            "⚠ 文件改了但没装上：{}。`komo config reload` 校验并装载",
            stale.join(" · ")
        )
    })
}

// ---- 小工具 ----

/// 一个可读的时刻。定时的那几条回执也印它，所以它是公开的——两处各写一遍
/// 格式，迟早有一处会和另一处不一样。
///
/// 本机时区的墙钟时刻，带偏移（`2026-09-25 14:00:00 +08:00`）。
///
/// 偏移必须印出来：这些输出也会被 agent 经 shell 读到，不带偏移的时刻会被当成本机时间。
/// 偏移按时刻本身取（夏令时两侧不同），所以走 jiff 的系统时区，不在启动时抓一个固定偏移；
/// `time` 的 `current_local_offset` 在多线程进程里还会直接拒绝。
pub fn stamp(at: OffsetDateTime) -> String {
    stamp_in(at, &jiff::tz::TimeZone::system())
}

fn stamp_in(at: OffsetDateTime, zone: &jiff::tz::TimeZone) -> String {
    match jiff::Timestamp::from_second(at.unix_timestamp()) {
        Ok(ts) => ts
            .to_zoned(zone.clone())
            .strftime("%Y-%m-%d %H:%M:%S %:z")
            .to_string(),
        Err(_) => at.to_string(),
    }
}

fn relative(at: OffsetDateTime, now: OffsetDateTime) -> String {
    let delta = at - now;
    let seconds = delta.whole_seconds();
    if seconds < 0 {
        return format!("已过 {}", human_span(-seconds));
    }
    format!("还有 {}", human_span(seconds))
}

fn human_span(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h{}m", seconds / 3600, (seconds % 3600) / 60)
    } else {
        format!("{}d", seconds / 86400)
    }
}

fn one_line(text: &str) -> String {
    let flat = text.replace('\n', " ⏎ ");
    if flat.chars().count() > 60 {
        format!("{}…", flat.chars().take(59).collect::<String>())
    } else {
        flat
    }
}

/// §7.5 的两类，如实显示。清单、提示行都要它，所以放在这里而不是各写一份。
pub fn intervention_kind_text(kind: InterventionKind) -> &'static str {
    match kind {
        InterventionKind::Verify => "结果不明",
        InterventionKind::Blocked => "阻塞",
        InterventionKind::Confirm => "待确认",
    }
}

/// 这一条此刻允许答复什么。**空表如实印成「（无）」**——自己推一个菜单会给出一个按下去
/// 没反应的答案（§11.3）。
pub fn verdicts_text(verdicts: &[InterventionVerdict]) -> String {
    if verdicts.is_empty() {
        return "（无）".into();
    }
    verdicts
        .iter()
        .map(|verdict| verdict.as_str())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// §6 / §8.4 的状态，如实显示。
///
/// `Waiting` 只有一个词，因为**"在等什么"是另一个维度**（[`WaitReason`]）：状态行把它
/// 接在后面印（见 [`wait_text`]），而不是在这里编出"等待干预 / 等待重试"几种状态。
pub fn status_text(state: RunState) -> &'static str {
    match state {
        RunState::Accepted => "已受理",
        RunState::Queued => "排队中",
        RunState::Running => "运行中",
        RunState::Waiting => "等待中",
        RunState::Completed => "已完成",
        RunState::Failed => "已失败",
        RunState::Cancelled => "已取消",
        RunState::Abandoned => "已放弃",
        RunState::Absorbed => "已并入",
    }
}

/// 「停着，而且在等什么」。§8.4：**排队二十分钟不知道为什么**正是这一格要答的问题。
///
/// `now` 由驱动传进来（状态机不读时钟）；`Retry` 那一句要算"还有多久"才算说得清。
pub fn wait_text(reason: &WaitReason, now: Option<OffsetDateTime>) -> String {
    match reason {
        // 句柄（Run ID）才是操作者要用的东西，而这一格只报"在等谁"：具体那一条在 §7.5 的
        // 清单里，状态行不复述它、也不替它编一个 id。
        //
        // `Approval` 只从 2026-10-11 之前的旧行解码出来（审批已删除）：Gateway 启动时按
        // "批准"续跑它，它不等人——如实说成马上续跑，不说"等审批"。
        WaitReason::Approval { .. } => "旧的审批等待，Gateway 会直接续跑".into(),
        WaitReason::Intervention { .. } => "等一条干预的答复".into(),
        WaitReason::Dependency { run } => format!("在等 Run {run}"),
        WaitReason::Retry {
            attempts,
            not_before,
            cause,
        } => {
            let remaining = match now {
                Some(now) if *not_before > now => {
                    format!("{} 后", wait_span(*not_before - now))
                }
                // 到点了却还停着：下一拍就会重试，别报一个负数的"还要等"。
                _ => "马上".into(),
            };
            format!(
                "{remaining}重试（{}，第 {attempts} 次）",
                retry_cause_text(*cause)
            )
        }
    }
}

/// 一次有界退避是**哪一种**失败（§8.5）：分类不是为了好看，是让人一眼知道该不该干预。
fn retry_cause_text(cause: RetryCause) -> &'static str {
    match cause {
        RetryCause::RateLimited => "限流",
        RetryCause::Transport => "连不上",
        RetryCause::Server => "服务端错误",
        RetryCause::Contended => "本地写争用",
    }
}

/// 一段时长的人话。
fn wait_span(duration: time::Duration) -> String {
    let seconds = duration.whole_seconds().max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

#[cfg(test)]
mod tests;
