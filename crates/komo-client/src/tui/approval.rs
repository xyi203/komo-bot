//! TUI 审批弹窗；答复打到 POST /v1/approvals/{id}/decision（§11.3、§13.5）。
//!
//! §7.2：「界面——聊天里的审批消息或 TUI 弹窗——显示具体动作、原因、改动和已有验证
//! 结果」，§11.3 的表把它列成**五项**：短 ID、动作、改动、原因、范围。这里的
//! [`approval_lines`] 就是那五项，一项一个小标题，缺一项是这个模块的 bug 而不是
//! 「那次没有」——`changes` 为空时印「（无）」，不是不印这一节。
//!
//! 答案不是藏在按键后面的，而是弹窗底部**列出来的一行行**（[`ApprovalModal::rows`]）：
//! `↑` / `↓` 移动高亮、`Enter` 确认，行首那个字母是直通键——习惯按 `y` 的人不必先移动
//! 高亮。菜单里只有 Policy 真给了的那些：`本次任务默认通过（本次 Run 范围）` 只在这条
//! 请求的 `scopes` 真的含 [`ApprovalScope::Run`] 时出现，`全部批准` 只在待处理多于一条
//! 时出现。**没有 `always`**：§7.2「不把『同意一次 Python』解释为『今后任意脚本均可执行』」，
//! 范围是 Policy 标出来的，不是按键变出来的。

use komo_kernel::protocol::http::ApprovalRecord;
use komo_kernel::types::chat::ApprovalScope;
use komo_kernel::types::plan::{ExecutionPlan, Operation, PlanSource, RecoveryMode, TargetAccess};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// 弹窗自己的状态：一条请求、一个滚动位置、一个高亮位置。
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalModal {
    pub record: ApprovalRecord,
    pub scroll: u16,
    /// 已经答过了，正在等服务端回执——再按一次不该发第二个请求。
    pub answering: bool,
    /// 菜单里高亮的那一行，[`ApprovalModal::rows`] 的下标。行数会随待处理条数变（批量
    /// 那行），所以读的时候一律过 [`ApprovalModal::selected_index`] 夹一遍。
    pub selected: usize,
}

impl ApprovalModal {
    pub fn new(record: ApprovalRecord) -> Self {
        ApprovalModal {
            record,
            scroll: 0,
            answering: false,
            selected: 0,
        }
    }

    /// 这条请求能不能批到「本次 Run 范围」。
    pub fn allows_run_scope(&self) -> bool {
        self.record.scopes.contains(&ApprovalScope::Run)
    }

    /// 弹窗底部那张菜单：**操作者能给的答案就是这些行**，没有藏在别处的键。
    ///
    /// 第一行是 `Enter` 的默认落点。顺序是刻意的：能范围化时它排第一（一次 `Enter` 就是
    /// 最常见的那一下），不能范围化时它就整个不出现，高亮自然落在「只批准本次调用」上。
    ///
    /// `approvals` 是**此刻待处理的审批条数**（含眼前这条）：批量那行答的是全部审批，
    /// 条数得摆在标签里——"全部"是 1 条还是 6 条，是按下去之前唯一要看清的事；只有这一条
    /// 时那行不列，因为此时它和「只批准本次调用」同义。
    ///
    /// 数的是审批而不是三类合计（§7.5）：批量只答审批（§11.3），把结果不明和阻塞也算进去，
    /// 那个数字就与按下去会发生的事不符。
    pub fn rows(&self, approvals: usize) -> Vec<ApprovalRow> {
        let mut rows = Vec::with_capacity(4);
        if self.allows_run_scope() {
            rows.push(ApprovalRow {
                key: 'r',
                label: "本次任务默认通过（本次 Run 范围）".to_string(),
                choice: ApprovalChoice::This(ApprovalAnswer::RUN),
            });
        }
        rows.push(ApprovalRow {
            key: 'y',
            label: "只批准本次调用".to_string(),
            choice: ApprovalChoice::This(ApprovalAnswer::ONCE),
        });
        rows.push(ApprovalRow {
            key: 'n',
            label: "拒绝（本次不执行）".to_string(),
            choice: ApprovalChoice::This(ApprovalAnswer::REJECT),
        });
        if approvals > 1 {
            rows.push(ApprovalRow {
                key: 'a',
                label: format!("全部批准（{approvals} 条，各按本次调用）"),
                choice: ApprovalChoice::AllPending,
            });
        }
        rows
    }

    /// 高亮那一行的下标，夹在 `approvals` 下菜单的行数里。
    pub fn selected_index(&self, approvals: usize) -> usize {
        self.selected
            .min(self.rows(approvals).len().saturating_sub(1))
    }

    /// `↑` / `↓`：移动高亮。到两头就停住，**不绕回去**——想按「拒绝」时多按一下不该跳回
    /// 「本次任务默认通过」。
    pub fn move_selection(&mut self, delta: i16, approvals: usize) {
        let moved = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.selected.saturating_add(delta as usize)
        };
        let last = self.rows(approvals).len().saturating_sub(1);
        self.selected = moved.min(last);
    }

    /// 边框底栏那一行：怎么开这张菜单，或者为什么现在按不动。菜单自己把每一行的答案与
    /// 直通键写在脸上，这一行不该再抄一遍。
    pub fn keys_hint(&self) -> &'static str {
        if self.answering {
            "已答复，等待服务端回执……"
        } else {
            "↑/↓ 选择 · Enter 确认 · Esc 拒绝"
        }
    }

    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(4);
    }

    pub fn scroll_down(&mut self) {
        self.scroll = self.scroll.saturating_add(4);
    }
}

/// 菜单里的一行：行首的直通键、印出来的字、按下去答的是什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRow {
    pub key: char,
    pub label: String,
    pub choice: ApprovalChoice,
}

/// 一行答案答的是谁。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalChoice {
    /// 眼前这一条（批还是拒、什么范围，都在 [`ApprovalAnswer`] 里）。
    This(ApprovalAnswer),
    /// **此刻待处理的全部**，各按本次调用（§11.3 的 `/approve all`）。
    AllPending,
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
                Span::raw(format!(" {}", target.describe())),
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
        Operation::Delegate { .. } => "派给子代理",
    }
}

/// 命令或代码本身——审批看的就是这一段。
fn operation_body(operation: &Operation) -> Option<String> {
    match operation {
        Operation::ShellCommand { command } => Some(command.clone()),
        Operation::PythonCall { module, function } => Some(format!("{module}.{function}()")),
        Operation::ToolboxChange { module } => Some(module.clone()),
        // 审批看的就是这一句任务正文：子代理拿到的**只有它**（§4）。续跑（§4）多写一句
        // "接着子 Run X"：目标进了计划、进了计划哈希，批的是"接着这一条"，换一条要重新问。
        Operation::Delegate { spec } => Some(match &spec.resumes {
            Some(target) => format!("接着子 Run {target}\n{}", spec.task),
            None => spec.task.clone(),
        }),
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

    /// 菜单就是全部答案，顺序固定：范围 → 本次 → 拒绝。
    #[test]
    fn the_menu_lists_every_answer_with_its_direct_key() {
        let modal = ApprovalModal::new(approval_record());
        let rows = modal.rows(1);
        let keys: Vec<char> = rows.iter().map(|row| row.key).collect();
        assert_eq!(keys, vec!['r', 'y', 'n'], "{rows:?}");
        assert_eq!(rows[0].choice, ApprovalChoice::This(ApprovalAnswer::RUN));
        assert_eq!(rows[1].choice, ApprovalChoice::This(ApprovalAnswer::ONCE));
        assert_eq!(rows[2].choice, ApprovalChoice::This(ApprovalAnswer::REJECT));
        assert!(rows[2].label.contains("拒绝"), "{rows:?}");
    }

    /// 范围那行只在 Policy 标了可范围化时出现——范围不是按键变出来的（§7.2）。
    #[test]
    fn the_run_row_is_offered_only_when_policy_marked_the_plan_scopable() {
        let mut record = approval_record();
        let modal = ApprovalModal::new(record.clone());
        assert!(modal.allows_run_scope());
        assert!(modal.rows(1)[0].label.contains("Run 范围"), "范围行排第一");

        record.scopes = vec![ApprovalScope::Once];
        let modal = ApprovalModal::new(record);
        assert!(!modal.allows_run_scope());
        assert!(
            !modal.rows(1).iter().any(|row| row.key == 'r'),
            "{:?}",
            modal.rows(1)
        );
        // 没有范围行时，`Enter` 的默认落点就是「只批准本次调用」。
        assert_eq!(
            modal.rows(1)[0].choice,
            ApprovalChoice::This(ApprovalAnswer::ONCE)
        );
    }

    /// 批量那行只在**真的还有别的**待处理时出现，而且带着条数（§11.3）。
    ///
    /// 一条时它和「只批准本次调用」同义，列出来只会让人以为还有什么没批；多条时不写条数，
    /// 按下去之前就不知道这一次要替几条计划签字。
    #[test]
    fn the_batch_row_appears_with_the_count_only_when_there_is_more_than_one() {
        let modal = ApprovalModal::new(approval_record());
        assert!(
            !modal
                .rows(1)
                .iter()
                .any(|row| row.choice == ApprovalChoice::AllPending),
            "{:?}",
            modal.rows(1)
        );

        let rows = modal.rows(3);
        let batch = rows.last().expect("批量那行");
        assert_eq!(batch.choice, ApprovalChoice::AllPending);
        assert_eq!(batch.key, 'a');
        assert!(batch.label.contains('3'), "{batch:?}");
        assert!(batch.label.contains("各按本次调用"), "{batch:?}");
    }

    /// 高亮到两头就停住：想按「拒绝」时多按一下不该跳回第一行。
    #[test]
    fn the_highlight_stops_at_both_ends() {
        let mut modal = ApprovalModal::new(approval_record());
        assert_eq!(modal.selected_index(1), 0, "默认落在第一行");
        for _ in 0..5 {
            modal.move_selection(1, 1);
        }
        assert_eq!(modal.selected_index(1), 2, "最后一行是拒绝");
        modal.move_selection(-1, 1);
        assert_eq!(modal.selected_index(1), 1);
        for _ in 0..9 {
            modal.move_selection(-1, 1);
        }
        assert_eq!(modal.selected_index(1), 0);

        // 待处理少到批量那行没了，高亮也不会指到不存在的行上。
        let mut modal = ApprovalModal::new(approval_record());
        modal.selected = 3;
        assert_eq!(modal.selected_index(1), 2);
        modal.move_selection(1, 1);
        assert_eq!(modal.selected_index(1), 2);
    }

    #[test]
    fn the_footer_hint_says_how_to_drive_the_menu() {
        let mut modal = ApprovalModal::new(approval_record());
        assert!(
            modal.keys_hint().contains("Enter 确认"),
            "{}",
            modal.keys_hint()
        );
        modal.answering = true;
        assert!(
            modal.keys_hint().contains("等待服务端回执"),
            "{}",
            modal.keys_hint()
        );
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

    /// 委派续跑（§4）：弹窗多写一句"接着子 Run X"，任务正文照旧在。
    #[test]
    fn a_delegate_resume_names_which_child_it_continues() {
        use komo_kernel::types::delegate::DelegateSpec;
        use komo_kernel::types::ids::{RunId, ToolCallId};

        let mut record = approval_record();
        record.plan.operation = Operation::Delegate {
            spec: DelegateSpec::new(
                RunId::from_raw("run-parent"),
                ToolCallId::from_raw("call-1"),
                "接着查 B",
            )
            .with_resumes(RunId::from_raw("run-child-1")),
        };
        let rendered = text_of(&approval_lines(&record));
        assert!(rendered.contains("接着子 Run run-child-1"), "{rendered}");
        assert!(rendered.contains("接着查 B"), "{rendered}");

        // 不续跑的普通委派不该多出这一句。
        record.plan.operation = Operation::Delegate {
            spec: DelegateSpec::new(
                RunId::from_raw("run-parent"),
                ToolCallId::from_raw("call-1"),
                "查 A",
            ),
        };
        let rendered = text_of(&approval_lines(&record));
        assert!(!rendered.contains("接着子 Run"), "{rendered}");
    }
}
