//! TUI 审批弹窗；答复打到 POST /v1/approvals/{id}/decision（§11.3、§13.5）。
//!
//! §7.2：「界面——聊天里的审批消息或 TUI 弹窗——显示具体动作、原因、改动和已有验证
//! 结果」，§11.3 的表把它列成**五项**：短 ID、动作、改动、原因、范围。这里的
//! [`approval_lines`] 就是那五项，一项一个小标题，缺一项是这个模块的 bug 而不是
//! 「那次没有」——`changes` 为空时印「（无）」，不是不印这一节。
//!
//! 按键只有三个答案：`y` 本次、`r` 本次 Run 范围、`n` / `Esc` 拒绝。**没有 `always`**：
//! §7.2「不把『同意一次 Python』解释为『今后任意脚本均可执行』」，而 `r` 也只在这条
//! 请求的 `scopes` 真的含 [`ApprovalScope::Run`] 时才生效——范围是 Policy 标出来的，
//! 不是按键变出来的。

use komo_kernel::protocol::http::ApprovalRecord;
use komo_kernel::types::chat::ApprovalScope;
use komo_kernel::types::plan::{ExecutionPlan, Operation, PlanSource, RecoveryMode, TargetAccess};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// 弹窗自己的状态：一条请求加一个滚动位置。
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalModal {
    pub record: ApprovalRecord,
    pub scroll: u16,
    /// 已经答过了，正在等服务端回执——再按一次不该发第二个请求。
    pub answering: bool,
}

impl ApprovalModal {
    pub fn new(record: ApprovalRecord) -> Self {
        ApprovalModal {
            record,
            scroll: 0,
            answering: false,
        }
    }

    /// 这条请求能不能批到「本次 Run 范围」。
    pub fn allows_run_scope(&self) -> bool {
        self.record.scopes.contains(&ApprovalScope::Run)
    }

    /// 底部那一行提示，按键随 `scopes` 与待处理条数变——列一个按下去没反应的键比不列
    /// 它更糟。
    ///
    /// `pending` 是**此刻待处理的全部条数**（含眼前这条）：`a` 答的是全部，条数得摆在
    /// 键旁边——"全部"是 1 条还是 6 条，是按下去之前唯一要看清的事。
    pub fn keys_hint(&self, pending: usize) -> String {
        let batch = if pending > 1 {
            format!("a 全部批准（{pending} 条，各按本次调用） · ")
        } else {
            // 只有这一条时 `a` 与 `y` 同义，列它只会让人以为还有什么没批。
            String::new()
        };
        if self.allows_run_scope() {
            format!("y 批准本次 · r 批准本次 Run 范围 · {batch}n / Esc 拒绝")
        } else {
            format!("y 批准本次 · {batch}n / Esc 拒绝　（这条请求不可范围化）")
        }
    }

    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(4);
    }

    pub fn scroll_down(&mut self) {
        self.scroll = self.scroll.saturating_add(4);
    }
}

/// 一次答复。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalAnswer {
    pub approved: bool,
    pub scope: ApprovalScope,
}

impl ApprovalAnswer {
    pub const ONCE: ApprovalAnswer = ApprovalAnswer {
        approved: true,
        scope: ApprovalScope::Once,
    };
    pub const RUN: ApprovalAnswer = ApprovalAnswer {
        approved: true,
        scope: ApprovalScope::Run,
    };
    pub const REJECT: ApprovalAnswer = ApprovalAnswer {
        approved: false,
        scope: ApprovalScope::Once,
    };
}

/// 弹窗正文：§11.3 的五项。
pub fn approval_lines(record: &ApprovalRecord) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    // 一、短 ID。
    lines.push(Line::from(vec![
        section("短 ID"),
        Span::styled(
            format!(" {}", record.short_id),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  ({})", record.approval),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    if let Some(until) = record.valid_until {
        lines.push(Line::from(Span::styled(
            format!("       有效期至 {until}"),
            Style::default().fg(Color::DarkGray),
        )));
    }

    // 二、动作：工具、命令 / 代码、真实目标路径、cwd、版本。
    lines.push(Line::from(section("动作")));
    lines.extend(plan_lines(&record.plan));

    // 三、改动：write / edit 的 diff（已截断），或 toolbox 启用的版本差异。
    lines.push(Line::from(section("改动")));
    match record.changes.as_deref() {
        Some(diff) if !diff.trim().is_empty() => lines.extend(diff_lines(diff)),
        _ => lines.push(dim("  （无）")),
    }
    if let Some(evidence) = record.evidence.as_deref().filter(|e| !e.trim().is_empty()) {
        lines.push(Line::from(section("已有验证结果")));
        for line in evidence.lines() {
            lines.push(Line::from(format!("  {line}")));
        }
    }

    // 四、原因：`PolicyDecision::Ask { reason }`。
    lines.push(Line::from(section("原因")));
    if record.reason.trim().is_empty() {
        lines.push(dim("  （未给出）"));
    } else {
        for line in record.reason.lines() {
            lines.push(Line::from(format!("  {line}")));
        }
    }

    // 五、范围。
    lines.push(Line::from(section("范围")));
    lines.push(Line::from(format!("  {}", scopes_text(&record.scopes))));

    lines
}

fn scopes_text(scopes: &[ApprovalScope]) -> String {
    if scopes.is_empty() {
        // §7.2：`Once` 总是在里面；一个空的 scopes 是服务端的问题，说出来而不是当成没有。
        return "本次调用（服务端没有给出范围清单）".to_string();
    }
    scopes
        .iter()
        .map(|scope| match scope {
            ApprovalScope::Once => "本次调用",
            ApprovalScope::Run => "本次 Run 范围",
            ApprovalScope::CronJob => "Cron Job（绑定 Job 版本）",
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// 「具体动作」那一节：工具、命令 / 代码、真实目标路径、cwd、版本。
pub fn plan_lines(plan: &ExecutionPlan) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        field("  工具"),
        Span::styled(plan.tool.clone(), Style::default().bold()),
        Span::raw(format!("  ·  {}", operation_name(&plan.operation))),
    ])];

    if let Some(body) = operation_body(&plan.operation) {
        for line in body.lines() {
            lines.push(Line::from(Span::styled(
                format!("    {line}"),
                Style::default().fg(Color::Cyan),
            )));
        }
    }

    lines.push(Line::from(vec![
        field("  来源"),
        Span::raw(source_text(&plan.source)),
    ]));

    match plan.cwd.as_ref() {
        Some(cwd) => lines.push(Line::from(vec![
            field("  cwd"),
            Span::raw(cwd.display().to_string()),
        ])),
        None => lines.push(Line::from(vec![field("  cwd"), dim_span("（未指定）")])),
    }

    if plan.targets.is_empty() {
        lines.push(Line::from(vec![
            field("  目标"),
            dim_span("（无文件目标）"),
        ]));
    } else {
        lines.push(Line::from(field("  目标")));
        for target in &plan.targets {
            let access = match target.access {
                TargetAccess::Read => "读",
                TargetAccess::Write => "写",
            };
            let mut spans = vec![
                Span::raw("    "),
                Span::styled(access.to_string(), Style::default().fg(Color::Magenta)),
                Span::raw(format!(" {}", target.path.display())),
            ];
            if let Some(version) = &target.expected_version {
                spans.push(Span::styled(
                    format!("  @{}", short_hash(version.as_str())),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            lines.push(Line::from(spans));
        }
    }

    let mut versions = Vec::new();
    if let Some(code) = &plan.versions.code {
        versions.push(format!("code {}", short_hash(code.as_str())));
    }
    if let Some(module) = &plan.versions.module {
        versions.push(format!("module {module}"));
    }
    if let Some(env) = &plan.versions.env {
        versions.push(format!("env {}", env.0));
    }
    lines.push(Line::from(vec![
        field("  版本"),
        if versions.is_empty() {
            dim_span("（无）")
        } else {
            Span::raw(versions.join(" · "))
        },
    ]));

    if !plan.resources.is_empty() {
        let names: Vec<String> = plan
            .resources
            .iter()
            .map(|r| match &r.credential_env {
                // 凭证只以变量名出现（§5.3）。
                Some(env) => format!("{}（凭证 ${env}）", r.name),
                None => r.name.clone(),
            })
            .collect();
        lines.push(Line::from(vec![
            field("  资源"),
            Span::raw(names.join(" · ")),
        ]));
    }

    lines.push(Line::from(vec![
        field("  恢复"),
        Span::raw(recovery_text(&plan.recovery)),
    ]));
    lines.push(Line::from(vec![
        field("  计划哈希"),
        Span::styled(
            short_hash(plan.plan_hash().as_str()),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    lines
}

fn operation_name(operation: &Operation) -> &'static str {
    match operation {
        Operation::ReadFile => "读文件",
        Operation::WriteFile => "写文件",
        Operation::ShellCommand { .. } => "shell 命令",
        Operation::PythonCode => "Python 代码",
        Operation::PythonCall { .. } => "调用已保存模块",
        Operation::ToolboxChange { .. } => "修改 toolbox",
        Operation::PythonEnvChange => "修改 Python 环境",
        Operation::MemoryChange => "记忆内部变更",
        Operation::PolicyChange => "修改权限 / Policy",
    }
}

/// 命令或代码本身——审批看的就是这一段。
fn operation_body(operation: &Operation) -> Option<String> {
    match operation {
        Operation::ShellCommand { command } => Some(command.clone()),
        Operation::PythonCall { module, function } => Some(format!("{module}.{function}()")),
        Operation::ToolboxChange { module } => Some(module.clone()),
        _ => None,
    }
}

fn source_text(source: &PlanSource) -> String {
    match source {
        PlanSource::Interactive { .. } => "交互请求".into(),
        PlanSource::Cron { job, job_version } => format!("Cron {job}（v{job_version}）"),
        PlanSource::Memory { .. } => "MemoryManager 内部变更".into(),
        PlanSource::Verification { of } => format!("恢复核对（{of}）"),
    }
}

fn recovery_text(recovery: &RecoveryMode) -> String {
    match recovery {
        RecoveryMode::SafeReread => "可安全重做的读取".into(),
        RecoveryMode::IdempotencyKey { key, .. } => format!("外部幂等键 {key}"),
        RecoveryMode::VerifyTarget => "可核对目标状态".into(),
        RecoveryMode::NoSafeRecovery => "无可靠恢复方式".into(),
    }
}

/// diff 着色。`+` 绿、`-` 红、`@@` 青，文件头加粗。
pub fn diff_lines(diff: &str) -> Vec<Line<'static>> {
    diff.lines()
        .map(|line| {
            let style = if line.starts_with("+++") || line.starts_with("---") {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if line.starts_with('+') {
                Style::default().fg(Color::Green)
            } else if line.starts_with('-') {
                Style::default().fg(Color::Red)
            } else if line.starts_with("@@") {
                Style::default().fg(Color::Cyan)
            } else if line.starts_with('\\') {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            Line::from(Span::styled(format!("  {line}"), style))
        })
        .collect()
}

fn section(name: &str) -> Span<'static> {
    Span::styled(
        format!("{name}："),
        Style::default()
            .fg(Color::LightBlue)
            .add_modifier(Modifier::BOLD),
    )
}

fn field(name: &str) -> Span<'static> {
    Span::styled(format!("{name}  "), Style::default().fg(Color::Gray))
}

fn dim(text: &str) -> Line<'static> {
    Line::from(dim_span(text))
}

fn dim_span(text: &str) -> Span<'static> {
    Span::styled(text.to_string(), Style::default().fg(Color::DarkGray))
}

fn short_hash(hash: &str) -> String {
    hash.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::approval_record;

    fn text_of(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_modal_shows_all_five_items() {
        let record = approval_record();
        let rendered = text_of(&approval_lines(&record));

        // 一、短 ID
        assert!(rendered.contains("短 ID"), "{rendered}");
        assert!(rendered.contains(record.short_id.as_str()), "{rendered}");
        // 二、动作：工具、命令、真实目标路径、cwd、版本
        assert!(rendered.contains("动作"), "{rendered}");
        assert!(rendered.contains("shell"), "{rendered}");
        assert!(rendered.contains("rm -rf build"), "{rendered}");
        assert!(rendered.contains("/home/u/project/build"), "{rendered}");
        assert!(rendered.contains("cwd"), "{rendered}");
        assert!(rendered.contains("/home/u/project"), "{rendered}");
        assert!(rendered.contains("版本"), "{rendered}");
        // 三、改动（diff）
        assert!(rendered.contains("改动"), "{rendered}");
        assert!(rendered.contains("-老的一行"), "{rendered}");
        assert!(rendered.contains("+新的一行"), "{rendered}");
        // 四、原因
        assert!(rendered.contains("原因"), "{rendered}");
        assert!(rendered.contains("命中 shell 规则"), "{rendered}");
        // 五、范围
        assert!(rendered.contains("范围"), "{rendered}");
        assert!(rendered.contains("本次调用"), "{rendered}");
        assert!(rendered.contains("本次 Run 范围"), "{rendered}");
    }

    #[test]
    fn an_empty_changes_field_still_gets_its_section() {
        let mut record = approval_record();
        record.changes = None;
        let rendered = text_of(&approval_lines(&record));
        assert!(rendered.contains("改动"), "{rendered}");
        assert!(rendered.contains("（无）"), "{rendered}");
    }

    #[test]
    fn a_diff_is_coloured_by_line_kind() {
        let lines = diff_lines("--- a\n+++ b\n@@ -1 +1 @@\n-老\n+新\n 不变");
        let colour = |index: usize| lines[index].spans[0].style.fg;
        assert_eq!(colour(2), Some(Color::Cyan), "@@ 是青色");
        assert_eq!(colour(3), Some(Color::Red), "- 是红色");
        assert_eq!(colour(4), Some(Color::Green), "+ 是绿色");
        assert_eq!(colour(5), None, "上下文行不着色");
        // `---` / `+++` 是文件头，不是删除 / 新增行。
        assert_eq!(colour(0), Some(Color::White));
    }

    #[test]
    fn the_run_key_is_offered_only_when_policy_marked_the_plan_scopable() {
        let mut record = approval_record();
        let modal = ApprovalModal::new(record.clone());
        assert!(modal.allows_run_scope());
        assert!(modal.keys_hint(1).contains('r'));

        record.scopes = vec![ApprovalScope::Once];
        let modal = ApprovalModal::new(record);
        assert!(!modal.allows_run_scope());
        assert!(modal.keys_hint(1).contains("不可范围化"));
    }

    /// 「全部批准」那个键只在**真的还有别的**待处理时出现，而且带着条数（§11.3）。
    ///
    /// 一条时它和 `y` 同义，列出来只会让人以为还有什么没批；多条时不写条数，按下去
    /// 之前就不知道这一次要替几条计划签字。
    #[test]
    fn the_batch_key_appears_with_the_count_only_when_there_is_more_than_one() {
        let modal = ApprovalModal::new(approval_record());
        let alone = modal.keys_hint(1);
        assert!(!alone.contains("全部批准"), "{alone}");

        let together = modal.keys_hint(3);
        assert!(together.contains("全部批准"), "{together}");
        assert!(together.contains('3'), "{together}");
        assert!(together.contains("各按本次调用"), "{together}");
    }

    #[test]
    fn a_credential_appears_only_as_a_variable_name() {
        let mut record = approval_record();
        record.plan.resources = vec![komo_kernel::types::plan::ResourceRef {
            name: "memos".into(),
            endpoint: Some("https://memos.example.com".into()),
            credential_env: Some("MEMOS_TOKEN".into()),
        }];
        let rendered = text_of(&approval_lines(&record));
        assert!(rendered.contains("$MEMOS_TOKEN"), "{rendered}");
    }
}
