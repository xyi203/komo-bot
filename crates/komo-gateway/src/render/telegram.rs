//! Telegram 内联按钮与消息渲染（§11.3）。
//!
//! 渠道之间的差别只在渲染，不在决策：这里把一个 [`Outbound`] 变成若干条待发送的
//! Telegram 消息，**不发送**、不认识 HTTP，也不知道 Session 是什么。
//!
//! 三件事定在这里，因为它们是同一个表的两面：
//!
//! - 审批请求的五项（短 ID / 动作 / 改动 / 原因 / 范围，§11.3 的 Telegram 列）；
//! - 决定按钮的回调负载 `approve:<short_id>` / `reject:<short_id>` 与它的**逆映射**
//!   `/approve <short_id>`——按钮回调与文本命令走同一个 `Dispatcher::handle`，所以
//!   回调只需变回那条文本命令（§11.3）。两个方向写在一个模块里，才不会有一天只改
//!   了一边；
//! - 4096 与 64 两个平台上限。前者按 **UTF-16 码元**计（Telegram 数的是码元，不是
//!   字节也不是 `char`），一个 emoji 占 2、一个汉字占 1。

use serde::Serialize;
use time::OffsetDateTime;

use komo_kernel::types::chat::{ApprovalPresentation, ApprovalScope, Outbound, PeerId};
use komo_kernel::types::ids::ShortId;
use komo_kernel::types::plan::{ExecutionPlan, Operation, TargetAccess};

/// 一条消息的上限，按 UTF-16 码元计。
pub const MESSAGE_LIMIT: usize = 4096;

/// `callback_data` 的上限（Bot API 硬限制，按字节计）。
pub const CALLBACK_DATA_LIMIT: usize = 64;

/// 动作块的截断长度（字符）。
const ACTION_LIMIT: usize = 1200;
/// 改动块的截断长度（字符）。§11.3：write / edit 的 diff **截断**。
const CHANGES_LIMIT: usize = 1200;
/// 原因引用块的截断长度（字符）。
const REASON_LIMIT: usize = 600;
/// 已有验证结果的截断长度（字符）。
const EVIDENCE_LIMIT: usize = 600;

const TRUNCATION_NOTE: &str = "…（已截断，完整内容见 TUI）";

/// Telegram 的两种解析模式里我们只用一种；`None` = 纯文本。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseMode {
    MarkdownV2,
}

impl ParseMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ParseMode::MarkdownV2 => "MarkdownV2",
        }
    }
}

/// 一个内联按钮。komo 只用 `callback_data` 这一种。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InlineButton {
    pub text: String,
    pub callback_data: String,
}

/// `reply_markup` 的线格式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InlineKeyboard {
    #[serde(rename = "inline_keyboard")]
    pub rows: Vec<Vec<InlineButton>>,
}

impl InlineKeyboard {
    /// 空键盘——决定之后用它把按钮去掉（§11.3）。
    pub fn empty() -> Self {
        Self { rows: Vec::new() }
    }
}

/// 一条渲染好的消息。`keyboard` 只在审批请求上有。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedMessage {
    pub text: String,
    pub parse_mode: Option<ParseMode>,
    pub keyboard: Option<InlineKeyboard>,
}

impl RenderedMessage {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            parse_mode: None,
            keyboard: None,
        }
    }

    pub fn markdown(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            parse_mode: Some(ParseMode::MarkdownV2),
            keyboard: None,
        }
    }

    /// 同一条消息的纯文本版本。MarkdownV2 被服务端拒绝时原样重发这一条（§13.2）。
    ///
    /// 只去掉 `parse_mode`，**不**去掉转义反斜杠：转义只在 MarkdownV2 里有意义，但
    /// 把它们从已经拼好的正文里摘出来需要重新解析一遍我们自己的输出，得不偿失，而
    /// 多出来的反斜杠不会让人读不懂一条已经失败过一次的消息。
    pub fn without_parse_mode(&self) -> Self {
        Self {
            text: self.text.clone(),
            parse_mode: None,
            keyboard: self.keyboard.clone(),
        }
    }
}

/// 一个 [`Outbound`] 渲染成的消息序列。超过 4096 的正文在这里就已经切好段。
///
/// 键盘挂在**最后一段**上：按钮属于整条请求，挂在被截在半途的第一段下面会让人以为
/// 那就是全部。
pub fn render(outbound: &Outbound) -> Vec<RenderedMessage> {
    match outbound {
        Outbound::Text { text } => markdown_segments(text, None),
        Outbound::RunFinished { summary, .. } => markdown_segments(summary, None),
        Outbound::NeedsAttention { reason, .. } => {
            markdown_segments(&format!("⚠️ 需要你判断\n\n{reason}"), None)
        }
        Outbound::ApprovalRequest(presentation) => markdown_segments(
            &approval_text(presentation),
            Some(approval_keyboard(presentation)),
        ),
        Outbound::ApprovalSettled {
            short_id,
            approved,
            by,
            at,
            ..
        } => vec![RenderedMessage::plain(settled_line(
            short_id, *approved, by, *at,
        ))],
    }
}

/// 决定按钮的回调负载。§11.3：回调里带的只是**定位**用的 id，不构成授权。
pub fn callback_data(approve: bool, short_id: &ShortId) -> String {
    let verb = if approve { "approve" } else { "reject" };
    format!("{verb}:{short_id}")
}

/// 回调负载 → 文本命令。按钮回调与文本命令走同一个 `Dispatcher::handle`（§11.3），
/// 所以这里只需把负载变回那条命令；认不出的负载返回 `None`，**不**编一条命令出来。
pub fn command_for_callback(data: &str) -> Option<String> {
    let (verb, raw) = data.split_once(':')?;
    let short_id = ShortId::parse(raw)?;
    match verb {
        "approve" => Some(format!("/approve {short_id}")),
        "reject" => Some(format!("/reject {short_id}")),
        _ => None,
    }
}

/// 审批请求的两个按钮。§11.3：**按钮只给"本次"**，范围授权用命令。
pub fn approval_keyboard(presentation: &ApprovalPresentation) -> InlineKeyboard {
    InlineKeyboard {
        rows: vec![vec![
            InlineButton {
                text: "批准".into(),
                callback_data: callback_data(true, &presentation.short_id),
            },
            InlineButton {
                text: "拒绝".into(),
                callback_data: callback_data(false, &presentation.short_id),
            },
        ]],
    }
}

/// 决定之后追加到原消息末尾的那一行："已批准 / 已拒绝 · 谁 · 何时"（§11.3）。
pub fn settled_line(short_id: &ShortId, approved: bool, by: &PeerId, at: OffsetDateTime) -> String {
    let verdict = if approved { "已批准" } else { "已拒绝" };
    format!("{verdict} · {short_id} · {by} · {}", format_time(at))
}

/// 审批请求的正文：§11.3 Telegram 列的五项。
pub fn approval_text(presentation: &ApprovalPresentation) -> String {
    let mut out = String::new();

    // 一、短 ID。
    out.push_str(&format!(
        "🔐 待审批 `{}`\n",
        escape_code(presentation.short_id.as_str())
    ));

    // 二、动作：工具、命令 / 代码、真实目标路径、cwd、版本。
    out.push_str(&format!(
        "\n*动作*\n{}",
        code_block(&truncate(&plan_action(&presentation.plan), ACTION_LIMIT))
    ));

    // 三、改动：write / edit 的 diff（截断）。
    if let Some(changes) = &presentation.changes {
        out.push_str(&format!(
            "\n*改动*\n{}",
            code_block(&truncate(changes, CHANGES_LIMIT))
        ));
    }

    // 已有验证结果（§7.2 要求界面显示它）。
    if let Some(evidence) = &presentation.evidence {
        out.push_str(&format!(
            "\n*已有验证*\n{}",
            code_block(&truncate(evidence, EVIDENCE_LIMIT))
        ));
    }

    // 四、原因：`PolicyDecision::Ask { reason }`，引用块。
    out.push_str(&format!(
        "\n*原因*\n{}",
        quote_block(&truncate(&presentation.reason, REASON_LIMIT))
    ));

    // 五、范围。
    out.push_str(&format!(
        "\n*范围*\n{}",
        escape_markdown_v2(&scope_line(presentation))
    ));

    if let Some(valid_until) = presentation.valid_until {
        out.push_str(&format!(
            "\n{}",
            escape_markdown_v2(&format!("有效期至 {}", format_time(valid_until)))
        ));
    }

    out
}

/// 按钮只给"本次"，其余范围写成命令（§11.3）。
fn scope_line(presentation: &ApprovalPresentation) -> String {
    let short_id = &presentation.short_id;
    let mut parts = vec!["按钮 = 本次调用".to_string()];
    for scope in &presentation.scopes {
        match scope {
            ApprovalScope::Once => {}
            ApprovalScope::Run => parts.push(format!("本次 Run：/approve {short_id} run")),
            // TODO(decide: Cron 范围授权在聊天里的写法文档没给，暂按 `run` 的形状推一个)
            ApprovalScope::CronJob => parts.push(format!("Cron Job：/approve {short_id} cron")),
        }
    }
    parts.join("；")
}

/// 动作块：§11.3 的"工具、命令 / 代码、真实目标路径、cwd、版本"。
fn plan_action(plan: &ExecutionPlan) -> String {
    let mut lines = vec![
        format!("工具: {}", plan.tool),
        format!("操作: {}", operation_label(&plan.operation)),
    ];
    match &plan.operation {
        Operation::ShellCommand { command } => lines.push(format!("命令: {command}")),
        Operation::PythonCall { module, function } => {
            lines.push(format!("调用: {module}.{function}"))
        }
        Operation::ToolboxChange { module } => lines.push(format!("模块: {module}")),
        Operation::PythonCode => {
            if let Some(code) = plan.args.get("code").and_then(|v| v.as_str()) {
                lines.push(format!("代码:\n{code}"));
            }
        }
        // 任务正文：子代理拿到的**只有它**（§4）。续跑（§4）多写一句"接着子 Run X"——
        // 目标进了计划、进了计划哈希，批的是"接着这一条"，换一条要重新问。
        Operation::Delegate { spec } => {
            lines.push(format!("任务: {}", spec.task));
            if let Some(target) = &spec.resumes {
                lines.push(format!("续跑: 接着子 Run {target}"));
            }
        }
        Operation::Dispatch { task, title } => {
            lines.push(format!("标题: {title}"));
            lines.push(format!("任务: {task}"));
        }
        Operation::Follow { task_id, text } => {
            lines.push(format!("任务: #{task_id}"));
            lines.push(format!("追问: {text}"));
        }
        _ => {}
    }
    if let Some(cwd) = &plan.cwd {
        lines.push(format!("cwd: {}", cwd.display()));
    }
    for target in &plan.targets {
        let access = match target.access {
            TargetAccess::Read => "读",
            TargetAccess::Write => "写",
        };
        lines.push(format!("{access}: {}", target.describe()));
    }
    if let Some(code) = &plan.versions.code {
        lines.push(format!("版本 code: {}", code.as_str()));
    }
    if let Some(module) = &plan.versions.module {
        lines.push(format!("版本 module: {module}"));
    }
    if let Some(env) = &plan.versions.env {
        lines.push(format!("版本 env: {}", env.0));
    }
    lines.join("\n")
}

fn operation_label(operation: &Operation) -> &'static str {
    match operation {
        Operation::ReadFile => "read_file",
        Operation::WriteFile => "write_file",
        Operation::ShellCommand { .. } => "shell_command",
        Operation::PythonCode => "python_code",
        Operation::PythonCall { .. } => "python_call",
        Operation::ToolboxChange { .. } => "toolbox_change",
        Operation::PythonEnvChange => "python_env_change",
        Operation::MemoryChange => "memory_change",
        Operation::PolicyChange => "policy_change",
        Operation::Delegate { .. } => "delegate",
        Operation::Dispatch { .. } => "dispatch",
        Operation::Follow { .. } => "follow",
    }
}

// ---------------------------------------------------------------- MarkdownV2

/// MarkdownV2 在普通文本里要求转义的全部字符（Bot API 文档逐字列出的那一串）。
const MARKDOWN_V2_RESERVED: &[char] = &[
    '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!', '\\',
];

/// 普通文本 → MarkdownV2 字面量。
pub fn escape_markdown_v2(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if MARKDOWN_V2_RESERVED.contains(&ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// 代码块内部只需要转义反引号与反斜杠（Bot API 文档）。
fn escape_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '`' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn code_block(body: &str) -> String {
    format!("```\n{}\n```\n", escape_code(body.trim_end_matches('\n')))
}

fn quote_block(body: &str) -> String {
    let mut out = String::new();
    for line in body.lines() {
        out.push('>');
        out.push_str(&escape_markdown_v2(line));
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str(">\n");
    }
    out
}

fn truncate(text: &str, limit: usize) -> String {
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= limit {
            out.push_str(TRUNCATION_NOTE);
            return out;
        }
        out.push(ch);
    }
    out
}

fn format_time(at: OffsetDateTime) -> String {
    let format = time::macros::format_description!(
        "[year]-[month]-[day] [hour]:[minute]:[second][offset_hour sign:mandatory]:[offset_minute]"
    );
    at.format(format)
        .unwrap_or_else(|_| at.unix_timestamp().to_string())
}

// ---------------------------------------------------------------- 分段

/// 把正文切成 ≤ [`MESSAGE_LIMIT`] 的若干段，键盘挂在最后一段。
///
/// 正文按 **MarkdownV2 原样**送出（`Outbound::Text` / `RunFinished` 的内容本来就是
/// 模型写的 markdown）。服务端拒绝时由发送方去掉 `parse_mode` 重发一次（§13.2），
/// 这里不预先判断哪段能过——判断得准就等于自己实现了一遍 Telegram 的解析器。
// TODO(decide: 要不要在这里做一遍 markdown → MarkdownV2 的方言转换。不转 = 多数模型
// 输出会走一次 400 再纯文本重发（多一个来回，但永远不丢消息）；转 = 少一个来回，但
// 转换器本身会成为一个要维护的解析器。文档只写了"失败回退纯文本重发"。)
fn markdown_segments(text: &str, keyboard: Option<InlineKeyboard>) -> Vec<RenderedMessage> {
    let mut messages: Vec<RenderedMessage> = split_message(text, MESSAGE_LIMIT)
        .into_iter()
        .map(RenderedMessage::markdown)
        .collect();
    if messages.is_empty() {
        messages.push(RenderedMessage::markdown(escape_markdown_v2("（空）")));
    }
    if let (Some(keyboard), Some(last)) = (keyboard, messages.last_mut()) {
        last.keyboard = Some(keyboard);
    }
    messages
}

/// UTF-16 码元数——Telegram 数的就是它。
pub fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// 按 [`MESSAGE_LIMIT`] 切段，尽量切在换行处。
pub fn split_message(text: &str, limit: usize) -> Vec<String> {
    assert!(limit > 0, "分段上限必须为正");
    if utf16_len(text) <= limit {
        let trimmed = text.trim_end();
        return if trimmed.is_empty() {
            Vec::new()
        } else {
            vec![trimmed.to_string()]
        };
    }

    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if utf16_len(rest) <= limit {
            let trimmed = rest.trim_end();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
            break;
        }
        // 限额内能放下的最大字节边界。
        let mut end = 0;
        let mut width = 0;
        for (index, ch) in rest.char_indices() {
            let next = width + ch.len_utf16();
            if next > limit {
                break;
            }
            width = next;
            end = index + ch.len_utf8();
        }
        // 宁可切在换行上——但不为此丢掉半段的容量。
        let cut = rest[..end]
            .rfind('\n')
            .map(|index| index + 1)
            .filter(|index| *index * 2 > end)
            .unwrap_or(end);
        let segment = rest[..cut].trim_end();
        if !segment.is_empty() {
            out.push(segment.to_string());
        }
        rest = &rest[cut..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use komo_kernel::types::chat::ChannelPlatform;
    use komo_kernel::types::ids::{ApprovalId, OperationId, RunId, SessionId};
    use komo_kernel::types::plan::{PlanSource, PlanTarget, PlanVersions, RecoveryMode};

    fn plan() -> ExecutionPlan {
        ExecutionPlan {
            operation_id: OperationId::from_raw("op-1"),
            source: PlanSource::Interactive {
                session: SessionId::from_raw("s-1"),
            },
            tool: "shell".into(),
            operation: Operation::ShellCommand {
                command: "rm -rf /tmp/scratch".into(),
            },
            run: Some(RunId::from_raw("run-1")),
            tool_call: None,
            args: serde_json::json!({ "command": "rm -rf /tmp/scratch" }),
            cwd: Some(PathBuf::from("/home/op/work")),
            targets: vec![PlanTarget::local(
                PathBuf::from("/tmp/scratch"),
                TargetAccess::Write,
            )],
            versions: PlanVersions::default(),
            resources: Vec::new(),
            recovery: RecoveryMode::NoSafeRecovery,
        }
    }

    fn presentation() -> ApprovalPresentation {
        ApprovalPresentation {
            approval: ApprovalId::from_raw("ap-1"),
            short_id: ShortId::parse("7K2M").unwrap(),
            plan_hash: plan().plan_hash(),
            plan: plan(),
            reason: "写入 workspace 之外的路径（第 3 条规则）".into(),
            changes: Some("--- a/x\n+++ b/x\n-1\n+2".into()),
            evidence: None,
            scopes: vec![ApprovalScope::Once, ApprovalScope::Run],
            valid_until: None,
        }
    }

    // ⑥ 审批消息含五项与两个按钮，callback_data ≤ 64 字节。
    #[test]
    fn an_approval_carries_the_five_items_and_two_buttons() {
        let messages = render(&Outbound::ApprovalRequest(Box::new(presentation())));
        assert_eq!(messages.len(), 1, "这条审批不该被切段");
        let message = &messages[0];

        // 一、短 ID。
        assert!(message.text.contains("7K2M"), "{}", message.text);
        // 二、动作（代码块里有工具、命令、cwd、真实目标路径）。
        assert!(message.text.contains("*动作*"), "{}", message.text);
        assert!(message.text.contains("工具: shell"), "{}", message.text);
        assert!(
            message.text.contains("rm -rf /tmp/scratch"),
            "{}",
            message.text
        );
        assert!(
            message.text.contains("cwd: /home/op/work"),
            "{}",
            message.text
        );
        assert!(
            message.text.contains("写: /tmp/scratch"),
            "{}",
            message.text
        );
        // 三、改动，代码块。
        assert!(message.text.contains("*改动*"), "{}", message.text);
        assert!(message.text.contains("--- a/x"), "{}", message.text);
        // 四、原因，引用块。
        assert!(message.text.contains("*原因*"), "{}", message.text);
        assert!(
            message.text.contains(">写入 workspace 之外的路径"),
            "{}",
            message.text
        );
        // 五、范围。
        assert!(message.text.contains("*范围*"), "{}", message.text);
        assert!(
            message.text.contains("按钮 \\= 本次调用"),
            "{}",
            message.text
        );
        assert!(
            message.text.contains("/approve 7K2M run"),
            "{}",
            message.text
        );

        let keyboard = message.keyboard.as_ref().expect("审批消息必须带按钮");
        assert_eq!(keyboard.rows.len(), 1);
        assert_eq!(keyboard.rows[0].len(), 2, "只有批准 / 拒绝两个按钮");
        assert_eq!(keyboard.rows[0][0].text, "批准");
        assert_eq!(keyboard.rows[0][1].text, "拒绝");
        for button in &keyboard.rows[0] {
            assert!(
                button.callback_data.len() <= CALLBACK_DATA_LIMIT,
                "callback_data 超过 64 字节：{}",
                button.callback_data
            );
        }
        assert_eq!(keyboard.rows[0][0].callback_data, "approve:7K2M");
        assert_eq!(keyboard.rows[0][1].callback_data, "reject:7K2M");
    }

    #[test]
    fn a_callback_payload_maps_back_to_the_text_command() {
        assert_eq!(
            command_for_callback("approve:7K2M").as_deref(),
            Some("/approve 7K2M")
        );
        assert_eq!(
            command_for_callback("reject:7K2M").as_deref(),
            Some("/reject 7K2M")
        );
        // 小写按 ShortId 的规范化收下。
        assert_eq!(
            command_for_callback("approve:7k2m").as_deref(),
            Some("/approve 7K2M")
        );
        // 认不出的负载不编一条命令出来。
        assert_eq!(command_for_callback("approve:ZZ"), None);
        assert_eq!(command_for_callback("drop:7K2M"), None);
        assert_eq!(command_for_callback("7K2M"), None);
    }

    #[test]
    fn the_keyboard_serializes_as_telegram_spells_it() {
        let keyboard = approval_keyboard(&presentation());
        let wire = serde_json::to_value(&keyboard).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "inline_keyboard": [[
                    { "text": "批准", "callback_data": "approve:7K2M" },
                    { "text": "拒绝", "callback_data": "reject:7K2M" },
                ]]
            })
        );
        assert_eq!(
            serde_json::to_value(InlineKeyboard::empty()).unwrap(),
            serde_json::json!({ "inline_keyboard": [] })
        );
    }

    // ⑧ 4096 分段。
    #[test]
    fn a_long_body_is_split_at_the_platform_limit() {
        let body = "x".repeat(MESSAGE_LIMIT * 2 + 7);
        let messages = render(&Outbound::Text { text: body.clone() });
        assert_eq!(messages.len(), 3);
        for message in &messages {
            assert!(utf16_len(&message.text) <= MESSAGE_LIMIT);
        }
        let rejoined: String = messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(rejoined, body, "分段不能丢字符");
    }

    #[test]
    fn splitting_counts_utf16_units_and_prefers_newlines() {
        // 汉字是一个码元，emoji 是两个——按 `char` 数会算错。
        assert_eq!(utf16_len("你好"), 2);
        assert_eq!(utf16_len("🙂"), 2);
        let segments = split_message("🙂🙂🙂", 2);
        assert_eq!(segments, vec!["🙂", "🙂", "🙂"]);

        let text = format!("{}\n{}", "a".repeat(60), "b".repeat(60));
        let segments = split_message(&text, 100);
        assert_eq!(segments, vec!["a".repeat(60), "b".repeat(60)]);
    }

    #[test]
    fn a_split_keeps_the_keyboard_on_the_last_segment() {
        let mut presentation = presentation();
        presentation.changes = Some("d".repeat(MESSAGE_LIMIT * 2));
        let messages = render(&Outbound::ApprovalRequest(Box::new(presentation)));
        // 改动块被截断了，所以其实不会切段——真被切段时键盘也只在最后一条上。
        assert!(messages.last().unwrap().keyboard.is_some());
        assert!(
            messages[..messages.len() - 1]
                .iter()
                .all(|m| m.keyboard.is_none())
        );
    }

    #[test]
    fn oversized_blocks_are_truncated_rather_than_split() {
        let mut presentation = presentation();
        presentation.changes = Some("d".repeat(CHANGES_LIMIT * 3));
        let messages = render(&Outbound::ApprovalRequest(Box::new(presentation)));
        assert_eq!(messages.len(), 1, "截断之后不该还需要分段");
        assert!(messages[0].text.contains(TRUNCATION_NOTE));
    }

    #[test]
    fn markdown_v2_escapes_every_reserved_character() {
        assert_eq!(escape_markdown_v2("a.b-c!"), "a\\.b\\-c\\!");
        assert_eq!(escape_markdown_v2("a\\b"), "a\\\\b");
        // 代码块里只有反引号与反斜杠要转义。
        assert_eq!(escape_code("a.b`c"), "a.b\\`c");
    }

    #[test]
    fn a_settled_line_names_the_verdict_the_person_and_the_time() {
        let line = settled_line(
            &ShortId::parse("7K2M").unwrap(),
            true,
            &PeerId::new("123456789"),
            OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
        );
        assert!(line.starts_with("已批准 · 7K2M · 123456789 · "), "{line}");
        let rejected = settled_line(
            &ShortId::parse("7K2M").unwrap(),
            false,
            &PeerId::new("1"),
            OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
        );
        assert!(rejected.starts_with("已拒绝 · "), "{rejected}");
    }

    #[test]
    fn a_settled_outbound_renders_as_a_plain_line() {
        let messages = render(&Outbound::ApprovalSettled {
            approval: ApprovalId::from_raw("ap-1"),
            short_id: ShortId::parse("7K2M").unwrap(),
            approved: false,
            by: PeerId::new("1"),
            at: OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
        });
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].parse_mode, None, "结论行不走 markdown");
        assert!(messages[0].keyboard.is_none());
    }

    #[test]
    fn a_run_finished_renders_its_summary() {
        let messages = render(&Outbound::RunFinished {
            session: SessionId::from_raw("s-1"),
            run: RunId::from_raw("run-1"),
            summary: "跑完了".into(),
        });
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "跑完了");
        assert_eq!(messages[0].parse_mode, Some(ParseMode::MarkdownV2));
    }

    #[test]
    fn dropping_the_parse_mode_keeps_the_text_and_the_keyboard() {
        let messages = render(&Outbound::ApprovalRequest(Box::new(presentation())));
        let plain = messages[0].without_parse_mode();
        assert_eq!(plain.parse_mode, None);
        assert_eq!(plain.text, messages[0].text);
        assert!(plain.keyboard.is_some());
    }

    #[test]
    fn a_peer_is_only_ever_rendered_as_platform_colon_chat() {
        // 渲染层不认识 Session，只认识 peer 的字面写法。
        assert_eq!(
            komo_kernel::types::chat::ChannelPeer::new(ChannelPlatform::Telegram, "42").to_string(),
            "telegram:42"
        );
    }

    /// 委派续跑（§4）：动作块多写一句"接着子 Run X"，任务正文照旧在。
    #[test]
    fn a_delegate_resume_names_which_child_it_continues() {
        use komo_kernel::types::delegate::DelegateSpec;

        let spec = DelegateSpec::new(
            RunId::from_raw("run-parent"),
            komo_kernel::types::ids::ToolCallId::from_raw("call-1"),
            "接着查 B",
        )
        .with_resumes(RunId::from_raw("run-child-1"));
        let action = plan_action(&ExecutionPlan {
            operation: Operation::Delegate { spec },
            tool: "delegate".into(),
            ..plan()
        });
        assert!(action.contains("任务: 接着查 B"), "{action}");
        assert!(action.contains("续跑: 接着子 Run run-child-1"), "{action}");

        let fresh_spec = DelegateSpec::new(
            RunId::from_raw("run-parent"),
            komo_kernel::types::ids::ToolCallId::from_raw("call-1"),
            "查 A",
        );
        let fresh = plan_action(&ExecutionPlan {
            operation: Operation::Delegate { spec: fresh_spec },
            tool: "delegate".into(),
            ..plan()
        });
        assert!(fresh.contains("任务: 查 A"), "{fresh}");
        assert!(!fresh.contains("续跑"), "{fresh}");
    }
}
