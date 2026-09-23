//! 微信纯文本审批渲染与短 ID 命令（§11.3）。
//!
//! 微信这一列与另外两个渠道的差别不在内容，在**能力**：iLink 只有纯文本，没有卡片、
//! 没有按钮、也没有"编辑已发出的那一条"。于是 §11.3 的表在这一列落成三条规矩：
//!
//! - **五项照发，但用纯文本**：短 ID / 动作 / 改动 / 原因 / 范围，没有代码块与引用块的
//!   标记——微信会把 ``` 原样显示出来，那只是噪音。
//! - **超长截断，并提示到 TUI 看全文**（§11.3 微信列的原话）。截断在**正文**上做，
//!   命令提示行在截断**之后**拼上去：一条被截断的审批仍然必须是可回答的，否则截断就
//!   把这条请求变成了死信。
//! - **范围只能用命令**：没有按钮，所以 `/approve <短 ID>` / `/reject <短 ID>` 这两条
//!   写在每条审批的末尾。`ApprovalSettled` 因此是**补发一行**结论，不是原地更新。
//!
//! 这里还住着入站去重键的回退哈希（[`fallback_request_key`]）。它和渲染放在一起不是
//! 凑巧：两者都是"把一条消息变成一段确定的文本"的纯函数，都要能在没有网络、没有
//! SDK 的情况下逐字断言。

use time::OffsetDateTime;

use komo_kernel::types::chat::{ApprovalPresentation, ApprovalScope, Outbound, PeerId};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::ShortId;
use komo_kernel::types::plan::{ExecutionPlan, Operation, TargetAccess};

/// 一条微信消息的正文上限，按**字符**计。
///
// TODO(decide: iLink 没有公开接口文档，服务端的真实上限未知。SDK 自己发长文本时按
// 4000 **字节**切块（`bot.rs::chunk_text`），所以 4000 字节是目前唯一有依据的数字；
// 这里取 1800 字符，对全中文正文约 5.4KB、对全 ASCII 约 1.8KB——比 4000 字节保守，
// 因为 §11.3 微信列要的是"截断并提示到 TUI"，宁可早截也不要发出去被服务端整条拒掉。)
pub const MESSAGE_LIMIT: usize = 1800;

/// 动作块的截断长度（字符）。
const ACTION_LIMIT: usize = 600;
/// 改动块的截断长度（字符）。§11.3：write / edit 的 diff **截断**。
const CHANGES_LIMIT: usize = 600;
/// 原因的截断长度（字符）。
const REASON_LIMIT: usize = 300;
/// 已有验证结果的截断长度（字符）。
const EVIDENCE_LIMIT: usize = 300;

/// 截断处留下的那句话。§11.3 微信列：**提示到 TUI 看全文**。
pub const TRUNCATION_NOTE: &str = "…（已截断，全文到 TUI 看）";

/// 回退去重键的时间窗，毫秒（§11.1）。
pub const FALLBACK_WINDOW_MS: i64 = 60_000;

/// 回退去重键里内容哈希取多少个十六进制位。
const FALLBACK_HASH_PREFIX: usize = 16;

/// 一个 [`Outbound`] 渲染成的那一段纯文本。
///
/// **只有一段**：微信这一列是截断，不是分段（§11.3）。分段会把一条审批拆成几条消息，
/// 而回答它的人只会看见最后一条。
pub fn render(outbound: &Outbound) -> String {
    match outbound {
        Outbound::Text { text } => truncate(text, MESSAGE_LIMIT),
        Outbound::RunFinished { summary, .. } => truncate(summary, MESSAGE_LIMIT),
        Outbound::NeedsAttention { reason, .. } => {
            truncate(&format!("⚠️ 需要你判断\n\n{reason}"), MESSAGE_LIMIT)
        }
        Outbound::ApprovalRequest(presentation) => approval_text(presentation),
        Outbound::ApprovalSettled {
            short_id,
            approved,
            by,
            at,
            ..
        } => settled_line(short_id, *approved, by, *at),
    }
}

/// 决定之后**补发**的那一行："已批准 / 已拒绝 · 谁 · 何时"（§11.3）。
///
/// 飞书原地更新卡片、Telegram 编辑原消息；微信两样都没有，所以它只能再说一句。旧消息
/// 上没有按钮可点，所以"决定过的请求还长着可点的按钮"这个问题在这里不存在。
pub fn settled_line(short_id: &ShortId, approved: bool, by: &PeerId, at: OffsetDateTime) -> String {
    let verdict = if approved { "已批准" } else { "已拒绝" };
    format!("{verdict} · {short_id} · {by} · {}", format_time(at))
}

/// 一条审批请求的全文：五项 + 两条命令提示。
///
/// **命令提示在截断之后拼**：正文可以被截掉，回答它的那两条命令不能。
pub fn approval_text(presentation: &ApprovalPresentation) -> String {
    let body = truncate(&approval_body(presentation), MESSAGE_LIMIT);
    format!("{body}\n\n{}", command_hint(&presentation.short_id))
}

/// 回答这条审批的路。§11.3 微信列：没有按钮，范围授权**用命令**。
///
/// **最短的那条放最前面**：待处理只有一条时，一个 `y` 就够了——手机上抄那 4 位短 ID
/// 才是真正的摩擦。短 ID 仍然写在里面：多条待处理、或者这条已经不在眼前时要用它。
pub fn command_hint(short_id: &ShortId) -> String {
    format!("回复 y 批准 · n 拒绝（要指明哪一条：/approve {short_id} · /reject {short_id}）")
}

/// 五项正文，未截断。
fn approval_body(presentation: &ApprovalPresentation) -> String {
    let mut out = String::new();

    // 一、短 ID。
    out.push_str(&format!("🔐 待审批 {}\n", presentation.short_id));

    // 二、动作：工具、命令 / 代码、真实目标路径、cwd、版本。
    out.push_str(&format!(
        "\n动作\n{}\n",
        truncate(&plan_action(&presentation.plan), ACTION_LIMIT)
    ));

    // 三、改动：write / edit 的 diff（截断）。
    if let Some(changes) = &presentation.changes {
        out.push_str(&format!("\n改动\n{}\n", truncate(changes, CHANGES_LIMIT)));
    }

    // 已有验证结果（§7.2 要求界面显示它）。
    if let Some(evidence) = &presentation.evidence {
        out.push_str(&format!(
            "\n已有验证\n{}\n",
            truncate(evidence, EVIDENCE_LIMIT)
        ));
    }

    // 四、原因：`PolicyDecision::Ask { reason }`。
    out.push_str(&format!(
        "\n原因\n{}\n",
        truncate(&presentation.reason, REASON_LIMIT)
    ));

    // 五、范围。
    out.push_str(&format!("\n范围\n{}", scope_line(presentation)));

    if let Some(valid_until) = presentation.valid_until {
        out.push_str(&format!("\n\n有效期至 {}", format_time(valid_until)));
    }

    out
}

/// 范围那一行。微信没有按钮，**每一种范围都是一条命令**（§11.3）。
fn scope_line(presentation: &ApprovalPresentation) -> String {
    let short_id = &presentation.short_id;
    let mut parts = vec![format!("本次调用：/approve {short_id}")];
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
    }
}

/// 超过 `limit` 个字符就截断，并在末尾写上 [`TRUNCATION_NOTE`]。
pub fn truncate(text: &str, limit: usize) -> String {
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

// ---------------------------------------------------------------- 去重键

/// `client_id` 为空时的回退去重键：`wechat:{from}:{内容哈希}:{60s 窗口}`（§11.1）。
///
/// iLink 的 wire 消息没有 `msg_id`，逐消息的唯一标识只有 `client_id`——而那是**发送方**
/// 微信客户端生成的 UUID，komo 既不能假设它全局唯一，也不能假设它非空（类型是
/// `String` 而不是 `Option<String>`）。空的时候只剩下三样东西是确定的：谁发的、说了
/// 什么、大概什么时候。
///
/// 窗口取 60 秒是有代价的，而且这个代价必须写下来：**同一个人在同一分钟里把同一句话
/// 说两遍，会被当成一次重投吞掉一条。** 这是刻意的取舍——微信的重投有两个来源（游标
/// 不持久化、服务端回了消息却回空游标），两者都会在秒级内把同一批消息原样再交一次，
/// 而"同一分钟内重复说同一句话"在聊天里既少见又无害（再说一遍即可）。反过来放宽窗口
/// 则会让一次真实的重投溜进 Run，那是真的执行两次。
///
/// 窗口是**定长的桶**（`create_time_ms / 60000`），不是滑动窗口，所以贴着桶边界的两条
/// 消息会落进不同的键。这对重投**不成问题**：`create_time_ms` 是 wire 上的字段，是消息
/// **原本**的发送时刻，重投时原样带回来，落的是同一个桶；只有服务端在重投时重写这个
/// 字段才会漏掉一次——而那时它连"同一条消息"都不成立了。
///
/// 哈希用 kernel 的 [`ContentHash`]（SHA-256），取前 16 位十六进制：键要进数据库、要
/// 进日志，全长 64 位只是噪音，而 64 bit 的碰撞面对"同一个人、同一分钟"这个前提已经
/// 远远够用。
pub fn fallback_request_key(from: &str, text: &str, create_time_ms: i64) -> String {
    let digest = ContentHash::of_str(text);
    let hash = &digest.as_str()[..FALLBACK_HASH_PREFIX.min(digest.as_str().len())];
    // `div_euclid` 而不是 `/`：create_time_ms 理论上可以是负数（服务端时钟乱跳），
    // 而截断除法在零两侧不单调，会让相邻的两毫秒落进同一个窗口号。
    let window = create_time_ms.div_euclid(FALLBACK_WINDOW_MS);
    format!("wechat:{from}:{hash}:{window}")
}

/// 主去重键：`wechat:{from_user_id}:{client_id}`（§11.1）。
pub fn primary_request_key(from: &str, client_id: &str) -> String {
    format!("wechat:{from}:{client_id}")
}

fn format_time(at: OffsetDateTime) -> String {
    let format = time::macros::format_description!(
        "[year]-[month]-[day] [hour]:[minute]:[second][offset_hour sign:mandatory]:[offset_minute]"
    );
    at.format(format)
        .unwrap_or_else(|_| at.unix_timestamp().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use komo_kernel::types::chat::ChannelPlatform;
    use komo_kernel::types::ids::{ApprovalId, OperationId, RunId, SessionId};
    use komo_kernel::types::plan::{
        PlanSource, PlanTarget, PlanVersions, RecoveryMode, TargetAccess,
    };

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
            short_id: ShortId::parse("7K2M").expect("短 ID"),
            plan_hash: plan().plan_hash(),
            plan: plan(),
            reason: "这条命令会删掉目录".into(),
            changes: Some("- old\n+ new".into()),
            evidence: Some("cargo test 通过".into()),
            scopes: vec![ApprovalScope::Once, ApprovalScope::Run],
            valid_until: None,
        }
    }

    // ④ 审批请求纯文本含五项与两条命令提示。
    #[test]
    fn an_approval_request_carries_five_items_and_the_two_commands() {
        let text = approval_text(&presentation());
        for item in ["7K2M", "动作", "改动", "原因", "范围"] {
            assert!(text.contains(item), "少了 {item}：{text}");
        }
        assert!(text.contains("已有验证"), "§7.2 要求显示已有验证：{text}");
        assert!(text.contains("/approve 7K2M"), "{text}");
        assert!(text.contains("/reject 7K2M"), "{text}");
        // 范围里的 Run 授权也是一条命令，不是按钮（§11.3 微信列）。
        assert!(text.contains("/approve 7K2M run"), "{text}");
        // 纯文本：不带 markdown 的代码块与转义。
        assert!(!text.contains("```"), "微信会把 ``` 原样显示：{text}");
        assert!(!text.contains('\\'), "纯文本里不该有转义反斜杠：{text}");
    }

    // ④（后半）ApprovalSettled 补发一行结论。
    #[test]
    fn a_decision_comes_back_as_one_more_line() {
        let settled = Outbound::ApprovalSettled {
            approval: ApprovalId::from_raw("ap-1"),
            short_id: ShortId::parse("7K2M").expect("短 ID"),
            approved: true,
            by: PeerId::new("wxid_op"),
            at: OffsetDateTime::from_unix_timestamp(1_760_000_000).expect("时间"),
        };
        let text = render(&settled);
        assert!(text.starts_with("已批准 · 7K2M · wxid_op · "), "{text}");
        assert_eq!(text.lines().count(), 1, "补发的是一行，不是一条新审批");

        let rejected = Outbound::ApprovalSettled {
            approval: ApprovalId::from_raw("ap-1"),
            short_id: ShortId::parse("7K2M").expect("短 ID"),
            approved: false,
            by: PeerId::new("wxid_op"),
            at: OffsetDateTime::from_unix_timestamp(1_760_000_000).expect("时间"),
        };
        assert!(render(&rejected).starts_with("已拒绝 · 7K2M · "));
    }

    // ⑤ 超长截断 + 「到 TUI 看全文」提示。
    #[test]
    fn an_oversized_body_is_truncated_with_a_pointer_to_the_tui() {
        let body = "长".repeat(MESSAGE_LIMIT * 2);
        let text = render(&Outbound::Text { text: body });
        assert!(text.ends_with(TRUNCATION_NOTE), "{text}");
        assert!(text.contains("TUI"), "提示必须说去哪看全文：{text}");
        assert_eq!(
            text.chars().count(),
            MESSAGE_LIMIT + TRUNCATION_NOTE.chars().count()
        );
    }

    // ⑤（后半）被截断的审批仍然是可回答的：命令提示在截断之后拼。
    #[test]
    fn a_truncated_approval_is_still_answerable() {
        let mut presentation = presentation();
        presentation.reason = "很".repeat(MESSAGE_LIMIT * 3);
        presentation.changes = Some("差".repeat(MESSAGE_LIMIT * 3));
        let text = approval_text(&presentation);
        assert!(text.contains(TRUNCATION_NOTE), "该截断了：{text}");
        // 提示行本身就带短 ID 与两条短答复；这里断言它是**按原样**落在末尾的那一段。
        let short = ShortId::parse("7K2M").expect("短 ID");
        assert!(
            text.ends_with(&command_hint(&short)),
            "截断不能把这条请求变成死信：{}",
            &text[text.len().saturating_sub(120)..]
        );
        assert!(text.ends_with('）'), "{text}");
    }

    #[test]
    fn a_short_body_is_left_alone() {
        let text = render(&Outbound::Text { text: "在".into() });
        assert_eq!(text, "在");
        assert!(!text.contains(TRUNCATION_NOTE));
    }

    #[test]
    fn a_run_summary_and_an_attention_notice_both_render() {
        let finished = Outbound::RunFinished {
            session: SessionId::from_raw("s-1"),
            run: RunId::from_raw("run-1"),
            summary: "跑完了".into(),
        };
        assert_eq!(render(&finished), "跑完了");

        let attention = Outbound::NeedsAttention {
            session: SessionId::from_raw("s-1"),
            run: RunId::from_raw("run-1"),
            reason: "磁盘满了".into(),
        };
        assert!(render(&attention).contains("磁盘满了"));
        assert!(render(&attention).contains("需要你判断"));
    }

    #[test]
    fn the_action_block_names_tool_command_cwd_and_targets() {
        let text = approval_text(&presentation());
        assert!(text.contains("工具: shell"), "{text}");
        assert!(text.contains("命令: rm -rf /tmp/scratch"), "{text}");
        assert!(text.contains("cwd: /home/op/work"), "{text}");
        assert!(text.contains("写: /tmp/scratch"), "{text}");
    }

    // ② 回退键：含 from + 内容哈希 + 60s 窗口；同一分钟内同文本同键、跨分钟不同键。
    #[test]
    fn the_fallback_key_is_sender_content_and_a_sixty_second_window() {
        // 一个整分的起点，好让"这一分钟之内"是一句确切的话。
        const MINUTE: i64 = FALLBACK_WINDOW_MS;
        const BASE: i64 = 29_333_333 * MINUTE;

        let a = fallback_request_key("wxid_op", "在吗", BASE);
        let b = fallback_request_key("wxid_op", "在吗", BASE + MINUTE - 1);
        assert_eq!(a, b, "同一分钟内同一句话是同一个键");

        let next_minute = fallback_request_key("wxid_op", "在吗", BASE + MINUTE);
        assert_ne!(a, next_minute, "跨分钟是另一个键");

        let other_text = fallback_request_key("wxid_op", "在么", BASE);
        assert_ne!(a, other_text, "换一句话是另一个键");

        let other_sender = fallback_request_key("wxid_x", "在吗", BASE);
        assert_ne!(a, other_sender, "换一个人是另一个键");

        assert!(a.starts_with("wechat:wxid_op:"), "{a}");
        let hash = ContentHash::of_str("在吗");
        assert!(a.contains(&hash.as_str()[..16]), "{a}");
        assert!(a.ends_with(&(BASE / MINUTE).to_string()), "{a}");
    }

    #[test]
    fn the_fallback_window_does_not_fold_across_zero() {
        // 服务端时钟乱跳到 1970 年之前时，截断除法会让 -1ms 与 +1ms 落进同一个窗口。
        assert_ne!(
            fallback_request_key("wxid_op", "在吗", -1),
            fallback_request_key("wxid_op", "在吗", 1)
        );
    }

    #[test]
    fn the_primary_key_is_sender_and_client_id() {
        assert_eq!(
            primary_request_key("wxid_op", "3f2b-uuid"),
            "wechat:wxid_op:3f2b-uuid"
        );
    }

    #[test]
    fn a_peer_is_a_wechat_user() {
        // 微信只有 DM：会话就是那个人（§11.2）。
        let peer = komo_kernel::types::chat::ChannelPeer::new(ChannelPlatform::Wechat, "wxid_op");
        assert_eq!(peer.to_string(), "wechat:wxid_op");
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
