//! 操作子命令的输出渲染（§3、§13.4）。
//!
//! 全是**纯函数**：协议类型进去，字符串出来。没有 I/O、没有时钟——「现在几点」由调用
//! 方传进来，否则 `cron list` 的「还有多久」就没法测。
//!
//! 一条贯穿的规矩：**不知道就说不知道**。`??` 是 uncertain 的记号（§8.6：副作用是否
//! 发生未知，不能在这里假装失败），`allowed by …` 只在真的拿到那条审批记录时才印。

use komo_kernel::cron::{FiringStatus, JobStatus, NotifyPolicy, OverlapPolicy, Trigger};
use komo_kernel::protocol::config::{ConfigIssue, IssueSeverity, SourceFile};
use komo_kernel::protocol::http::{
    ApprovalListResponse, ApprovalRecord, ConfigCheckResponse, ConfigReloadResponse,
    CronListResponse, HealthResponse, IndexState, MemoryIndexStatus, MemoryListResponse,
    ModelsResponse, RunDetail, SessionListResponse, SessionSummary, ToolCallSummary,
};
use komo_kernel::types::memory::{
    Confirmation, MemoryItem, MemoryKind, MemoryScope, MemoryState, Provenance,
};
use komo_kernel::types::status::{RunStatus, ToolCallState};
use time::OffsetDateTime;

use crate::tui::app::status_text;

/// `komo session list`。
pub fn session_list(response: &SessionListResponse) -> String {
    if response.sessions.is_empty() {
        return "没有会话".into();
    }
    let mut out = Vec::new();
    out.push(format!(
        "{:<38} {:<10} {:<22} {}",
        "SESSION", "状态", "更新时间", "标题"
    ));
    for session in &response.sessions {
        out.push(format!(
            "{:<38} {:<10} {:<22} {}",
            session.session,
            session.current_status.map(status_text).unwrap_or("空闲"),
            stamp(session.updated_at),
            session_title(session)
        ));
    }
    out.join("\n")
}

fn session_title(session: &SessionSummary) -> String {
    match (&session.title, &session.workdir) {
        (title, Some(workdir)) if title.is_empty() => format!("（无标题） · {workdir}"),
        (title, Some(workdir)) => format!("{title} · {workdir}"),
        (title, None) if title.is_empty() => "（无标题）".into(),
        (title, None) => title.clone(),
    }
}

/// `komo run inspect RUN_ID`。
///
/// `approvals` 是与这个 Run 相关的审批记录，用来印「allowed by …」——调用方用
/// `GET /v1/approvals` 加 `ApprovalListQuery { run: Some(run), include_decided: true, .. }`
/// 取来：那条审批早就不在待处理集合里了。给不出就**不印**，印一个猜的放行来源比不印更糟。
pub fn run_inspect(detail: &RunDetail, approvals: &[ApprovalRecord]) -> String {
    let mut out = Vec::new();
    let summary = &detail.summary;
    out.push(format!("Run    {}", summary.run));
    out.push(format!("会话   {}", summary.session));
    out.push(format!(
        "状态   {}{}",
        status_text(summary.status),
        match summary.status {
            RunStatus::Interrupted => "（上一个执行实例没有收尾，不等于用户取消）",
            RunStatus::NeedsAttention => "（等操作者判断）",
            _ => "",
        }
    ));
    out.push(format!("轮数   {}", summary.rounds));
    out.push(format!(
        "开始   {}{}",
        stamp(summary.created_at),
        match summary.ended_at {
            Some(ended) => format!("   结束 {}", stamp(ended)),
            None => String::new(),
        }
    ));

    if detail.calls.is_empty() {
        out.push("调用   （无）".into());
    } else {
        out.push("调用".into());
        for call in &detail.calls {
            out.push(format!("  {}", call_line(call)));
            if let Some(approval) = approvals
                .iter()
                .find(|record| record.call.as_ref() == Some(&call.call))
            {
                out.push(format!("      {}", approval_provenance(approval)));
            }
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

    if let Some(message) = &detail.final_message {
        out.push("最终回复".into());
        for line in message.lines() {
            out.push(format!("  {line}"));
        }
    }
    out.join("\n")
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

/// 「这一步是谁放行的」。
fn approval_provenance(record: &ApprovalRecord) -> String {
    match &record.decision {
        Some(decision) => {
            let verdict = if decision.approved {
                "allowed by"
            } else {
                "denied by"
            };
            let who = decision
                .by
                .as_ref()
                .map(|peer| peer.to_string())
                .unwrap_or_else(|| "操作者".into());
            let scope = match decision.scope {
                komo_kernel::types::chat::ApprovalScope::Once => "本次调用",
                komo_kernel::types::chat::ApprovalScope::Run => "本次 Run 范围",
                komo_kernel::types::chat::ApprovalScope::CronJob => "Cron Job",
            };
            format!(
                "{verdict} {who}（{scope}，{}）· 审批 {}",
                stamp(decision.decided_at),
                record.short_id
            )
        }
        None => format!("等待审批 {}（{}）", record.short_id, record.reason),
    }
}

/// `komo approval list`。
pub fn approval_list(response: &ApprovalListResponse) -> String {
    if response.approvals.is_empty() {
        return "没有待处理的审批".into();
    }
    let mut out = vec![format!(
        "{:<6} {:<10} {:<38} {}",
        "短ID", "工具", "RUN", "原因"
    )];
    for record in &response.approvals {
        out.push(format!(
            "{:<6} {:<10} {:<38} {}",
            record.short_id,
            record.plan.tool,
            record
                .run
                .as_ref()
                .map(|r| r.to_string())
                .unwrap_or_else(|| "—".into()),
            record.reason
        ));
    }
    out.push(String::new());
    out.push("`komo approval approve <短ID> [run]` / `komo approval reject <短ID>`".into());
    out.join("\n")
}

/// `komo approval show <id>`：与 TUI 弹窗**同一组内容**（§11.3 的五项）。
pub fn approval_show(record: &ApprovalRecord) -> String {
    let mut out = Vec::new();
    out.push(format!(
        "短 ID   {}  ({})",
        record.short_id, record.approval
    ));
    if let Some(until) = record.valid_until {
        out.push(format!("有效期  至 {}", stamp(until)));
    }
    out.push(String::new());
    out.push("动作".into());
    for line in crate::tui::approval::plan_lines(&record.plan) {
        out.push(flatten(&line));
    }
    out.push(String::new());
    out.push("改动".into());
    match record.changes.as_deref().filter(|c| !c.trim().is_empty()) {
        Some(diff) => out.extend(diff.lines().map(|l| format!("  {l}"))),
        None => out.push("  （无）".into()),
    }
    if let Some(evidence) = record.evidence.as_deref().filter(|e| !e.trim().is_empty()) {
        out.push(String::new());
        out.push("已有验证结果".into());
        out.extend(evidence.lines().map(|l| format!("  {l}")));
    }
    out.push(String::new());
    out.push("原因".into());
    out.push(format!("  {}", record.reason));
    out.push(String::new());
    out.push("范围".into());
    let scopes: Vec<&str> = record
        .scopes
        .iter()
        .map(|scope| match scope {
            komo_kernel::types::chat::ApprovalScope::Once => "本次调用",
            komo_kernel::types::chat::ApprovalScope::Run => "本次 Run 范围",
            komo_kernel::types::chat::ApprovalScope::CronJob => "Cron Job",
        })
        .collect();
    out.push(format!(
        "  {}",
        if scopes.is_empty() {
            "本次调用".to_string()
        } else {
            scopes.join(" · ")
        }
    ));
    if let Some(decision) = &record.decision {
        out.push(String::new());
        out.push(format!("已决定  {}", approval_provenance(record)));
        let _ = decision;
    }
    out.join("\n")
}

fn flatten(line: &ratatui::text::Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// `komo cron list`。
///
/// 每一行是"这个 Job 是什么"，下一行是"它上一次怎么样了"——§10 的
/// 「Cron 的结果去原 Session 查看」需要先知道有没有结果、是哪一个 Session。
pub fn cron_list(response: &CronListResponse, now: OffsetDateTime) -> String {
    if response.jobs.is_empty() {
        return "没有定时任务".into();
    }
    let mut out = vec![format!(
        "{:<38} {:<10} {:<8} {:<8} {:<22} {}",
        "JOB", "状态", "重叠", "投递", "下次", "名称 / 触发"
    )];
    for job in &response.jobs {
        out.push(format!(
            "{:<38} {:<10} {:<8} {:<8} {:<22} {} · {}",
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
        ));
        if let Some(last) = response
            .status
            .iter()
            .find(|status| status.job == job.id)
            .and_then(|status| status.last.as_ref())
        {
            // 「更新本次触发状态」（§10）：ok / error / waiting / skipped 都要看得见，
            // 否则一个天天被跳过的 Job 和一个天天跑成的 Job 长得一样。
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
            // 一个再也不响的 Job 应当在清单里看得见。
            out.push(format!("  ⚠ {error}"));
        }
    }
    out.join("\n")
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
        // 观察时间与入库时间是两回事（§9.2）。
        format!(
            "观察    {}   入库 {}   更新 {}",
            stamp(item.observed_at),
            stamp(item.created_at),
            stamp(item.updated_at)
        ),
    ];
    if let Some(until) = item.valid_until {
        out.push(format!("有效期  至 {}", stamp(until)));
    }
    // 取代关系是前向链：读到的人手里拿着的正是新的这一条（§9.6）。
    if let Some(superseded) = &item.supersedes {
        out.push(format!(
            "取代      {}@{}",
            superseded.memory, superseded.revision
        ));
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
        "使用    {} 次{}",
        item.usage.count,
        item.usage
            .last_used_at
            .map(|at| format!("，最近 {}", stamp(at)))
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
    }
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
    out.join("\n")
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

/// `komo model list`：每个模型支持哪几档 effort（§13.3）。
pub fn model_list(response: &ModelsResponse) -> String {
    if response.models.is_empty() {
        return "没有可选模型".into();
    }
    let mut out = vec![format!("{:<30} {:<14} {}", "MODEL", "PROVIDER", "EFFORT")];
    for entry in &response.models {
        let efforts = if entry.efforts.is_empty() {
            // 空表 = 一档都不支持，不是「还不知道」。
            "（不接受显式 effort）".to_string()
        } else {
            entry
                .efforts
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join(" · ")
        };
        out.push(format!(
            "{:<30} {:<14} {efforts}{}",
            entry.id,
            entry.provider,
            if entry.default { "   ← 当前" } else { "" }
        ));
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

/// 一个可读的时刻。`komo cron` 那几条回执也印它，所以它是公开的——两处各写一遍
/// 格式，迟早有一处会和另一处不一样。
pub fn stamp(at: OffsetDateTime) -> String {
    let format = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    at.format(&format).unwrap_or_else(|_| at.to_string())
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

#[cfg(test)]
mod tests;
