//! 飞书卡片与按钮回调的渲染（§11.3）。
//!
//! 渠道之间的差别只在渲染，不在决策：这里把一个 [`Outbound`] 变成若干条待发送的飞书
//! 消息（`msg_type` + `content` 字符串），**不发送**、不认识 HTTP，也不知道 Session
//! 是什么。
//!
//! 三件事定在这里，因为它们是同一个表的两面：
//!
//! - 审批卡片的五项（短 ID / 动作 / 改动 / 原因 / 范围，§11.3 的飞书列）：标题带短 ID，
//!   动作是 markdown 代码块，改动进折叠区，原因是备注；
//! - 按钮 `value` 与它的**逆映射**——按钮回调与文本命令走同一个 `Dispatcher::handle`
//!   （§11.3），所以回调只需变回那条文本命令。两个方向写在一个模块里，才不会有一天
//!   只改了一边；
//! - 决定之后的那张**无按钮卡片**：PATCH 换的是整张卡（飞书没有"只改按钮"的接口），
//!   所以它是由原卡片派生出来的，原来的五项还得读得到。
//!
//! 卡片用 **JSON 2.0**（`"schema": "2.0"`）：`collapsible_panel`（折叠区）只在 2.0 里
//! 有，而 §11.3 的飞书列写的就是"卡片折叠区"。`config.update_multi` 两个版本都有，且
//! PATCH 要求更新前后都为 `true`（spike callbacks.md 3d）。

use serde_json::{Value, json};
use time::OffsetDateTime;

use komo_kernel::types::chat::{ApprovalPresentation, ApprovalScope, Outbound, PeerId};
use komo_kernel::types::ids::ShortId;
use komo_kernel::types::plan::{ExecutionPlan, Operation, TargetAccess};

/// 一条文本消息的分段上限，按 `char` 计。
///
/// 飞书给的硬上限是 `content` 序列化后 150 KB，没有给字符数；这里取一个远低于它、又
/// 不至于把一段正常回答切碎的值。
// TODO(decide: 官方只给了 150KB 的 content 上限，没有给"一条消息多少字合适"。3000 是
// 保守取值，真机看过排版后再定。)
pub const MESSAGE_LIMIT: usize = 3000;

/// 动作块的截断长度（字符）。
const ACTION_LIMIT: usize = 1200;
/// 改动块的截断长度（字符）。§11.3：write / edit 的 diff **截断**。
const CHANGES_LIMIT: usize = 1200;
/// 原因备注的截断长度（字符）。
const REASON_LIMIT: usize = 600;
/// 已有验证结果的截断长度（字符）。
const EVIDENCE_LIMIT: usize = 600;

const TRUNCATION_NOTE: &str = "…（已截断，完整内容见 TUI）";

/// 按钮 `value` 里的动作名。回调负载回传的就是它。
const ACTION_APPROVE: &str = "approve";
const ACTION_REJECT: &str = "reject";

/// 一条渲染好的飞书消息。
///
/// `content` 已经是**字符串**：飞书的 `im/v1/messages` 收的 `content` 是一个 JSON
/// 字符串而不是 JSON 对象，序列化的时机定在渲染这一侧，发送方只管把它放进 body。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedMessage {
    /// `text` / `interactive`。
    pub msg_type: &'static str,
    pub content: String,
}

impl RenderedMessage {
    /// 一条纯文本消息。
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            msg_type: "text",
            content: json!({ "text": text.into() }).to_string(),
        }
    }

    /// 一条交互卡片消息。
    pub fn card(card: &Value) -> Self {
        Self {
            msg_type: "interactive",
            content: card.to_string(),
        }
    }

    pub fn is_card(&self) -> bool {
        self.msg_type == "interactive"
    }
}

/// 一个 [`Outbound`] 渲染成的消息序列。超过 [`MESSAGE_LIMIT`] 的正文在这里就已经切好段。
///
/// 审批请求永远是**一条**卡片：卡片里的每一块都已经按自己的上限截断，切段会把按钮和
/// 它说明的动作分到两条消息上。
pub fn render(outbound: &Outbound) -> Vec<RenderedMessage> {
    match outbound {
        Outbound::Text { text } => text_segments(text),
        Outbound::RunFinished { summary, .. } => text_segments(summary),
        Outbound::NeedsAttention { reason, .. } => {
            text_segments(&format!("⚠️ 需要你判断\n\n{reason}"))
        }
        Outbound::ApprovalRequest(presentation) => {
            vec![RenderedMessage::card(&approval_card(presentation))]
        }
        Outbound::ApprovalSettled {
            short_id,
            approved,
            by,
            at,
            ..
        } => vec![RenderedMessage::card(&settled_card(
            None, short_id, *approved, by, *at,
        ))],
    }
}

/// 一段正文切成若干条文本消息。空正文也要有一条——否则"回执"会变成什么都没发生。
pub fn text_segments(text: &str) -> Vec<RenderedMessage> {
    let segments = split_message(text, MESSAGE_LIMIT);
    if segments.is_empty() {
        return vec![RenderedMessage::text("（空）")];
    }
    segments.into_iter().map(RenderedMessage::text).collect()
}

// ---------------------------------------------------------------- 按钮的双向映射

/// 决定按钮的 `value`。§11.3：回调里带的只是**定位**用的 id，不构成授权。
pub fn button_value(approve: bool, short_id: &ShortId) -> Value {
    json!({
        "action": if approve { ACTION_APPROVE } else { ACTION_REJECT },
        "short_id": short_id.as_str(),
    })
}

/// 回调负载 → 文本命令。
///
/// 按钮回调与文本命令走同一个 `Dispatcher::handle`（§11.3），所以这里只需把 `value`
/// 变回那条命令；认不出的负载返回 `None`，**不**编一条命令出来。
///
/// 收下两种形状：飞书把 `action.value` 回传成对象，但同一个字段在一些卡片版本 / 一些
/// 客户端上是**字符串**；两种都只是同一份 JSON 的两种包装，认一种会在某个早上静默失灵。
pub fn command_for_action(value: &Value) -> Option<String> {
    let owned;
    let object = match value {
        Value::Object(_) => value,
        Value::String(raw) => {
            owned = serde_json::from_str::<Value>(raw).ok()?;
            &owned
        }
        _ => return None,
    };
    let action = object.get("action")?.as_str()?;
    let short_id = ShortId::parse(object.get("short_id")?.as_str()?)?;
    match action {
        ACTION_APPROVE => Some(format!("/approve {short_id}")),
        ACTION_REJECT => Some(format!("/reject {short_id}")),
        _ => None,
    }
}

// ---------------------------------------------------------------- 卡片

/// 审批请求的卡片：§11.3 飞书列的五项 + 两个按钮。
pub fn approval_card(presentation: &ApprovalPresentation) -> Value {
    let short_id = &presentation.short_id;
    let mut elements = vec![
        // 二、动作：markdown 代码块。
        markdown(format!(
            "**动作**\n{}",
            code_block(&truncate(&plan_action(&presentation.plan), ACTION_LIMIT))
        )),
    ];

    // 三、改动：折叠区。
    if let Some(changes) = &presentation.changes {
        elements.push(collapsible(
            "**改动**",
            &code_block(&truncate(changes, CHANGES_LIMIT)),
        ));
    }

    // 已有验证结果（§7.2 要求界面显示它）。也进折叠区：它通常比改动还长。
    if let Some(evidence) = &presentation.evidence {
        elements.push(collapsible(
            "**已有验证**",
            &code_block(&truncate(evidence, EVIDENCE_LIMIT)),
        ));
    }

    // 四、原因：卡片备注。
    elements.push(note(format!(
        "原因：{}",
        truncate(&presentation.reason, REASON_LIMIT)
    )));

    // 五、范围。
    elements.push(markdown(format!("**范围**\n{}", scope_line(presentation))));

    if let Some(valid_until) = presentation.valid_until {
        elements.push(note(format!("有效期至 {}", format_time(valid_until))));
    }

    // 按钮之外，把**文本命令**也写出来。
    //
    // 按钮要应用在开放平台开通卡片回调、并且回调真的投得到；而 ws 那一条路在 openlark
    // 0.20.0 上对 `card` 帧是直接丢（`frame_handler.rs` 的 `"card" => skip`，见 §14），
    // 所以"按钮点了没反应"是一种真实状态。文本命令走 `im.message.receive_v1`，不受它
    // 影响——这句话是那张卡片在按钮不可用时**唯一**还指得出的路。
    elements.push(note(format!(
        "也可以直接回：y 批准 · n 拒绝（要指明哪一条：/approve {short_id} · /reject {short_id}）"
    )));

    // 两个按钮。§11.3：**按钮只给"本次"**，范围授权用命令。
    elements.push(actions(vec![
        button("批准", "primary", button_value(true, short_id)),
        button("拒绝", "danger", button_value(false, short_id)),
    ]));

    card(
        format!("🔐 待审批 · {short_id}"),
        "orange",
        Value::Array(elements),
    )
}

/// 决定之后那张**无按钮**的卡片（§11.3：决定过的请求不该还长着可点的按钮）。
///
/// 飞书没有"只去掉按钮"的接口，PATCH 换的是整张卡，所以它由原卡片派生：去掉按钮那一块
/// 、换掉标题、在末尾追加结论。原卡片不在手上时（重启之后那份进程内的账没了）退化成
/// 一张只有结论的卡。
pub fn settled_card(
    original: Option<&Value>,
    short_id: &ShortId,
    approved: bool,
    by: &PeerId,
    at: OffsetDateTime,
) -> Value {
    let verdict = verdict_word(approved);
    let template = if approved { "green" } else { "grey" };
    let line = settled_line(short_id, approved, by, at);

    let mut elements: Vec<Value> = original
        .and_then(|card| card.pointer("/body/elements"))
        .and_then(Value::as_array)
        .map(|elements| {
            elements
                .iter()
                .filter(|element| {
                    // 按 `element_id` 认按钮那一块：按钮嵌在 `column_set` 的 `column`
                    // 里，按 `tag` 找要递归，而按 id 是一句话。
                    element.get("element_id").and_then(Value::as_str) != Some(ACTIONS_ID)
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    elements.push(note(line));

    card(
        format!("{verdict} · {short_id}"),
        template,
        Value::Array(elements),
    )
}

/// 决定那一行："已批准 / 已拒绝 · 短 ID · 谁 · 何时"（§11.3）。
pub fn settled_line(short_id: &ShortId, approved: bool, by: &PeerId, at: OffsetDateTime) -> String {
    format!(
        "{} · {short_id} · {by} · {}",
        verdict_word(approved),
        format_time(at)
    )
}

fn verdict_word(approved: bool) -> &'static str {
    if approved { "已批准" } else { "已拒绝" }
}

/// 卡片骨架。
///
/// `config.update_multi` 必须为 `true`：PATCH 要求**更新前后**两张卡都带着它
/// （spike callbacks.md 3d）。它写在这一个函数里，所以两张卡不可能只有一张带。
fn card(title: String, template: &str, elements: Value) -> Value {
    json!({
        "schema": "2.0",
        "config": { "update_multi": true, "width_mode": "fill" },
        "header": {
            "title": { "tag": "plain_text", "content": title },
            "template": template,
        },
        "body": { "elements": elements },
    })
}

fn markdown(content: impl Into<String>) -> Value {
    json!({ "tag": "markdown", "content": content.into() })
}

/// 按钮那一块的 `element_id`。`settled_card` 靠它把按钮整块摘掉。
const ACTIONS_ID: &str = "approval_actions";

/// 一行小字："原因：…"、"有效期至 …"、以及结论那一行。
///
/// **卡片 2.0 里没有 `note` 组件。** 飞书的 2.0 不兼容变更写得很直接：「2.0 结构不再
/// 支持 note 组件与 action 模块（`tag` 为 `action`）」，并给出替代写法——普通文本组件
/// 加 `notation` 字号与灰色。这件事的后果不是"样式差一点"：带 `note` 的卡片被平台整张
/// 打回（`230099 / 200861 unsupported tag note`），**一条审批都到不了聊天里**，而聊天
/// 正是审批的主入口（§11.3）。而 2.0 对不认识的属性是**报错**而不是忽略，所以这不是
/// 可以留着的风格问题。
fn note(content: impl Into<String>) -> Value {
    json!({
        "tag": "div",
        "text": {
            "tag": "plain_text",
            "content": content.into(),
            "text_size": "notation",
            "text_color": "grey",
        },
    })
}

/// 并排的按钮。**2.0 里没有 `action` 模块**（与 `note` 一起被去掉），替代写法是按钮
/// 组件加容器：`column_set` 一列一个，各占一半宽。
fn actions(buttons: Vec<Value>) -> Value {
    json!({
        "tag": "column_set",
        "element_id": ACTIONS_ID,
        "flex_mode": "none",
        "horizontal_spacing": "default",
        "columns": buttons
            .into_iter()
            .map(|button| json!({
                "tag": "column",
                "width": "weighted",
                "weight": 1,
                "elements": [button],
            }))
            .collect::<Vec<_>>(),
    })
}

/// 折叠区。默认收起——改动是给"想看"的人准备的，不是每次都要读的。
fn collapsible(title: &str, body: &str) -> Value {
    json!({
        "tag": "collapsible_panel",
        "expanded": false,
        "header": { "title": { "tag": "markdown", "content": title } },
        "elements": [markdown(body)],
    })
}

/// 一个回传交互按钮。卡片 2.0 的回传写在 `behaviors` 里。
fn button(text: &str, kind: &str, value: Value) -> Value {
    json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": text },
        "type": kind,
        "behaviors": [{ "type": "callback", "value": value }],
    })
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
            if let Some(code) = plan.args.get("code").and_then(Value::as_str) {
                lines.push(format!("代码:\n{code}"));
            }
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
        lines.push(format!("{access}: {}", target.path.display()));
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
    }
}

// ---------------------------------------------------------------- 文本工具

/// markdown 代码块。
///
/// 围栏比正文里最长的一串反引号**长一个**：一份 diff 里出现 ``` 是常事（它可能正是
/// 一段 markdown 的改动），而定长三反引号会让块在那里提前收尾，后半段改动就变成了
/// 卡片正文。改正文是另一条路——那会让人读到一份与磁盘上不一样的 diff。
fn code_block(body: &str) -> String {
    let body = body.trim_end_matches('\n');
    let longest_run = body
        .split(|ch| ch != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    let fence = "`".repeat(longest_run.saturating_add(1).max(3));
    format!("{fence}\n{body}\n{fence}")
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

/// 按 `limit` 个 `char` 切段，尽量切在换行处。
///
/// 飞书数的是 `content` 的**字节**（150 KB），不是码元也不是字符；这里按 `char` 切是
/// 因为 `MESSAGE_LIMIT` 本来就是一个远低于硬上限的排版取值，用哪种单位都不会撞线。
pub fn split_message(text: &str, limit: usize) -> Vec<String> {
    assert!(limit > 0, "分段上限必须为正");
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if rest.chars().count() <= limit {
            let trimmed = rest.trim_end();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
            break;
        }
        // 限额内能放下的最大字节边界。
        let end = rest
            .char_indices()
            .nth(limit)
            .map(|(index, _)| index)
            .unwrap_or(rest.len());
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
            args: json!({ "command": "rm -rf /tmp/scratch" }),
            cwd: Some(PathBuf::from("/home/op/work")),
            targets: vec![PlanTarget {
                path: PathBuf::from("/tmp/scratch"),
                access: TargetAccess::Write,
                expected_version: None,
            }],
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

    fn body(card: &Value) -> &Vec<Value> {
        card.pointer("/body/elements")
            .and_then(Value::as_array)
            .expect("卡片有 body.elements")
    }

    /// 整张卡片序列化之后的文本，断言"里面有没有这几个字"用。
    fn flat(card: &Value) -> String {
        card.to_string()
    }

    // ⑤ 审批卡片含五项、两个按钮、update_multi: true。
    #[test]
    fn an_approval_card_carries_the_five_items_and_two_buttons() {
        let messages = render(&Outbound::ApprovalRequest(Box::new(presentation())));
        assert_eq!(messages.len(), 1, "审批永远是一条卡片");
        assert_eq!(messages[0].msg_type, "interactive");
        let card: Value = serde_json::from_str(&messages[0].content).expect("content 是卡片 JSON");

        // update_multi：PATCH 要求更新前后都带（spike 3d）。
        assert_eq!(card["config"]["update_multi"], json!(true));
        assert_eq!(card["schema"], json!("2.0"));

        // 一、短 ID 在标题里。
        assert_eq!(
            card["header"]["title"]["content"],
            json!("🔐 待审批 · 7K2M")
        );

        let elements = body(&card);
        // 二、动作：markdown 代码块。
        let action = elements[0]["content"].as_str().expect("动作是 markdown");
        assert!(action.starts_with("**动作**"), "{action}");
        assert!(action.contains("```"), "动作要在代码块里：{action}");
        assert!(action.contains("工具: shell"), "{action}");
        assert!(action.contains("rm -rf /tmp/scratch"), "{action}");
        assert!(action.contains("cwd: /home/op/work"), "{action}");
        assert!(action.contains("写: /tmp/scratch"), "{action}");

        // 三、改动：折叠区。
        let panel = elements
            .iter()
            .find(|e| e["tag"] == json!("collapsible_panel"))
            .expect("改动进折叠区");
        assert_eq!(panel["header"]["title"]["content"], json!("**改动**"));
        assert_eq!(panel["expanded"], json!(false));
        assert!(flat(panel).contains("--- a/x"), "{}", flat(panel));

        // 四、原因：小字（2.0 里由"普通文本 + notation 字号"替代 note 组件）。
        let reason = elements
            .iter()
            .find(|e| e["tag"] == json!("div") && e["text"]["text_size"] == json!("notation"))
            .expect("原因是小字");
        assert!(
            flat(reason).contains("原因：写入 workspace 之外的路径"),
            "{}",
            flat(reason)
        );

        // 五、范围。
        let scope = elements
            .iter()
            .filter_map(|e| e["content"].as_str())
            .find(|c| c.starts_with("**范围**"))
            .expect("范围");
        assert!(scope.contains("按钮 = 本次调用"), "{scope}");
        assert!(scope.contains("/approve 7K2M run"), "{scope}");

        // 两个按钮，只有批准 / 拒绝。
        let actions = elements
            .last()
            .expect("最后一块是按钮")
            .get("columns")
            .and_then(Value::as_array)
            .expect("按钮那一块是 column_set");
        assert_eq!(actions.len(), 2);
        let buttons: Vec<&Value> = actions
            .iter()
            .map(|column| &column["elements"][0])
            .collect();
        assert_eq!(buttons[0]["text"]["content"], json!("批准"));
        assert_eq!(buttons[1]["text"]["content"], json!("拒绝"));
        assert_eq!(
            buttons[0]["behaviors"][0]["value"],
            json!({ "action": "approve", "short_id": "7K2M" })
        );
        assert_eq!(
            buttons[1]["behaviors"][0]["value"],
            json!({ "action": "reject", "short_id": "7K2M" })
        );
        assert_eq!(buttons[0]["behaviors"][0]["type"], json!("callback"));
    }

    /// **卡片里不许有 2.0 去掉的那两个 tag。**
    ///
    /// 飞书的 2.0 不兼容变更：「2.0 结构不再支持 note 组件与 action 模块（`tag` 为
    /// `action`）」，而且 2.0 对不认识的组件是**整张卡打回**而不是忽略。这一条在线上
    /// 发生过一次：每一条审批请求都被平台拒（`230099 / 200861 unsupported tag note`），
    /// 于是**审批一条都到不了聊天里**——审批的主入口整个是死的，而失败只落在网关日志的
    /// 一行 WARN 上。
    ///
    /// 按 `tag` 递归找，不只看第一层：以后它们长到哪一层都得拦住。
    #[test]
    fn a_card_carries_no_component_the_2_0_schema_dropped() {
        let asked = approval_card(&presentation());
        let settled = settled_card(
            Some(&asked),
            &ShortId::parse("7K2M").unwrap(),
            true,
            &PeerId::new("ou_op"),
            OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
        );
        for card in [&asked, &settled] {
            let mut pending = vec![card.clone()];
            while let Some(node) = pending.pop() {
                match node {
                    Value::Object(map) => {
                        let tag = map.get("tag").and_then(Value::as_str);
                        assert_ne!(tag, Some("note"), "2.0 没有 note：{}", flat(card));
                        assert_ne!(tag, Some("action"), "2.0 没有 action：{}", flat(card));
                        pending.extend(map.values().cloned());
                    }
                    Value::Array(items) => pending.extend(items),
                    _ => {}
                }
            }
        }
    }

    /// **卡片上必须有文本命令**：按钮不是唯一的答复方式，也不能当它是。
    ///
    /// 按钮要应用在开放平台开通卡片回调、并且回调投得到；ws 那一条路上 openlark 0.20.0
    /// 对 `card` 帧是直接丢（`"card" => skip`，见 §14），所以"按钮点了没反应"是一种真实
    /// 状态。那一刻这张卡片上唯一还指得出的路就是这行命令——它走
    /// `im.message.receive_v1`，与卡片回调无关。
    #[test]
    fn an_approval_card_also_carries_the_text_command() {
        let flat = flat(&approval_card(&presentation()));
        assert!(flat.contains("/approve 7K2M"), "{flat}");
        assert!(flat.contains("/reject 7K2M"), "{flat}");
    }

    #[test]
    fn an_approval_card_without_changes_has_no_panel() {
        let mut presentation = presentation();
        presentation.changes = None;
        let card = approval_card(&presentation);
        assert!(
            !body(&card)
                .iter()
                .any(|e| e["tag"] == json!("collapsible_panel")),
            "没有改动就不该有空折叠区"
        );
    }

    #[test]
    fn evidence_gets_its_own_panel() {
        let mut presentation = presentation();
        presentation.evidence = Some("3 passed".into());
        let card = approval_card(&presentation);
        let titles: Vec<_> = body(&card)
            .iter()
            .filter(|e| e["tag"] == json!("collapsible_panel"))
            .map(|e| e["header"]["title"]["content"].clone())
            .collect();
        assert_eq!(titles, vec![json!("**改动**"), json!("**已有验证**")]);
    }

    #[test]
    fn a_valid_until_shows_up_as_small_text() {
        let mut presentation = presentation();
        presentation.valid_until =
            Some(OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap());
        let card = approval_card(&presentation);
        assert!(
            flat(&card).contains("有效期至 2025-10-09"),
            "{}",
            flat(&card)
        );
    }

    // 按钮 value 的双向映射。
    #[test]
    fn a_button_value_maps_back_to_the_text_command() {
        let approve = button_value(true, &ShortId::parse("7K2M").unwrap());
        assert_eq!(
            command_for_action(&approve).as_deref(),
            Some("/approve 7K2M")
        );
        let reject = button_value(false, &ShortId::parse("7K2M").unwrap());
        assert_eq!(command_for_action(&reject).as_deref(), Some("/reject 7K2M"));
    }

    #[test]
    fn a_value_delivered_as_a_string_is_read_too() {
        let raw = json!(r#"{"action":"approve","short_id":"7k2m"}"#);
        // 小写按 ShortId 的规范化收下。
        assert_eq!(command_for_action(&raw).as_deref(), Some("/approve 7K2M"));
    }

    #[test]
    fn an_unrecognized_value_invents_no_command() {
        assert_eq!(
            command_for_action(&json!({ "action": "drop", "short_id": "7K2M" })),
            None
        );
        assert_eq!(command_for_action(&json!({ "action": "approve" })), None);
        assert_eq!(
            command_for_action(&json!({ "action": "approve", "short_id": "ZZ" })),
            None
        );
        assert_eq!(command_for_action(&json!(null)), None);
        assert_eq!(command_for_action(&json!("not json")), None);
        assert_eq!(command_for_action(&json!(["approve"])), None);
    }

    // ⑥ 决定之后的卡片：没有按钮，原来的五项还在，标题换成结论。
    #[test]
    fn a_settled_card_drops_the_buttons_and_keeps_the_rest() {
        let original = approval_card(&presentation());
        let settled = settled_card(
            Some(&original),
            &ShortId::parse("7K2M").unwrap(),
            true,
            &PeerId::new("ou_op"),
            OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
        );

        assert_eq!(settled["config"]["update_multi"], json!(true));
        assert_eq!(
            settled["header"]["title"]["content"],
            json!("已批准 · 7K2M")
        );
        assert!(
            !body(&settled)
                .iter()
                .any(|e| e["element_id"] == json!(ACTIONS_ID)),
            "决定过的请求不该还长着可点的按钮"
        );
        let flat = flat(&settled);
        assert!(flat.contains("已批准 · 7K2M · ou_op · "), "{flat}");
        assert!(flat.contains("工具: shell"), "原来的五项还得读：{flat}");
        assert!(flat.contains("--- a/x"), "{flat}");
    }

    #[test]
    fn a_settled_card_without_the_original_is_still_a_card() {
        let settled = settled_card(
            None,
            &ShortId::parse("7K2M").unwrap(),
            false,
            &PeerId::new("ou_op"),
            OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
        );
        assert_eq!(
            settled["header"]["title"]["content"],
            json!("已拒绝 · 7K2M")
        );
        assert_eq!(body(&settled).len(), 1);
        assert!(flat(&settled).contains("已拒绝 · 7K2M · ou_op · "));
    }

    #[test]
    fn a_settled_outbound_renders_as_a_card() {
        let messages = render(&Outbound::ApprovalSettled {
            approval: ApprovalId::from_raw("ap-1"),
            short_id: ShortId::parse("7K2M").unwrap(),
            approved: false,
            by: PeerId::new("ou_op"),
            at: OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
        });
        assert_eq!(messages.len(), 1);
        assert!(messages[0].is_card());
    }

    #[test]
    fn a_settled_line_names_the_verdict_the_person_and_the_time() {
        let line = settled_line(
            &ShortId::parse("7K2M").unwrap(),
            true,
            &PeerId::new("ou_op"),
            OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
        );
        assert!(line.starts_with("已批准 · 7K2M · ou_op · "), "{line}");
    }

    // ⑧ 超长分段。
    #[test]
    fn a_long_body_is_split_at_the_platform_limit() {
        let text = "x".repeat(MESSAGE_LIMIT * 2 + 7);
        let messages = render(&Outbound::Text { text: text.clone() });
        assert_eq!(messages.len(), 3);
        let mut rejoined = String::new();
        for message in &messages {
            assert_eq!(message.msg_type, "text");
            let content: Value = serde_json::from_str(&message.content).unwrap();
            let part = content["text"].as_str().unwrap();
            assert!(part.chars().count() <= MESSAGE_LIMIT);
            rejoined.push_str(part);
        }
        assert_eq!(rejoined, text, "分段不能丢字符");
    }

    #[test]
    fn splitting_prefers_newlines_and_never_loses_characters() {
        let text = format!("{}\n{}", "a".repeat(60), "b".repeat(60));
        assert_eq!(
            split_message(&text, 100),
            vec!["a".repeat(60), "b".repeat(60)]
        );
        // 汉字按 char 计。
        assert_eq!(split_message("你好世界", 2), vec!["你好", "世界"]);
    }

    #[test]
    fn an_empty_body_still_produces_one_message() {
        let messages = render(&Outbound::Text { text: "   ".into() });
        assert_eq!(messages.len(), 1);
        assert!(messages[0].content.contains("（空）"));
    }

    #[test]
    fn oversized_blocks_are_truncated_rather_than_split() {
        let mut presentation = presentation();
        presentation.changes = Some("d".repeat(CHANGES_LIMIT * 3));
        let messages = render(&Outbound::ApprovalRequest(Box::new(presentation)));
        assert_eq!(messages.len(), 1);
        assert!(messages[0].content.contains(TRUNCATION_NOTE));
    }

    #[test]
    fn a_fence_inside_the_body_cannot_close_the_code_block() {
        let mut presentation = presentation();
        presentation.changes = Some("+```\n+rm -rf /".into());
        let card = approval_card(&presentation);
        let panel = body(&card)
            .iter()
            .find(|e| e["tag"] == json!("collapsible_panel"))
            .unwrap();
        let content = panel["elements"][0]["content"].as_str().unwrap();
        assert!(
            content.starts_with("````\n"),
            "围栏要比正文里的长：{content}"
        );
        assert!(content.ends_with("\n````"), "{content}");
        assert!(content.contains("+```\n"), "正文一个字都不改：{content}");
    }

    #[test]
    fn a_run_finished_renders_its_summary() {
        let messages = render(&Outbound::RunFinished {
            session: SessionId::from_raw("s-1"),
            run: RunId::from_raw("run-1"),
            summary: "跑完了".into(),
        });
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, json!({ "text": "跑完了" }).to_string());
    }

    #[test]
    fn needs_attention_says_so_up_front() {
        let messages = render(&Outbound::NeedsAttention {
            session: SessionId::from_raw("s-1"),
            run: RunId::from_raw("run-1"),
            reason: "工具结果不确定".into(),
        });
        assert!(messages[0].content.contains("需要你判断"));
    }
}
