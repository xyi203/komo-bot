//! `edit` / `write` 的 `then_run`：改完文件紧接着跑一条命令，一次调用、一条结果（§4）。
//!
//! 两步**合并授权、没有半截**（kernel 的组合计划，§7.1）。执行上照 SoL-Pi 的 Action Fusion：
//!
//! - 改动失败 → 命令不跑，结果里写 `[then_run:skipped]`；
//! - 改动之后目标的内容哈希必须还是改动的结果版本——中间被别人动过，命令同样不跑；
//! - 命令走 `shell` 自己的执行（进程组、时限收紧到活动执行时限、取消），输出流进**同一次
//!   尝试**的 stdout / stderr；
//! - 命令非零退出是结果、不是回滚：改动留着，整次调用记 Failed，退出码照实给。
//!
//! 恢复：带 `then_run` 的计划是 `NoSafeRecovery`。命令可能又改过这份文件，改动自己的
//! 内容哈希核对因此不再证明什么——started 而无结果一律交给人（§8.6）。

use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::plan::{ApprovedPlan, ExecutionPlan, RecoveryMode};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{ToolContext, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::shell::{ShellArgs, plan_for_call};
use super::write::target_path;
use super::{FileVersion, current};

pub const THEN_RUN_SUCCEEDED: &str = "[then_run:succeeded]";
pub const THEN_RUN_FAILED: &str = "[then_run:failed]";
pub const THEN_RUN_SKIPPED: &str = "[then_run:skipped]";

/// 模型给的 `then_run`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThenRun {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// `then_run` 的参数 schema。`what` 是改动的叫法（"编辑" / "写入"）。
pub fn schema(what: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": format!(
            "{what}成功之后紧接着对这个文件跑的命令——运行、构建、启动/重启、安装或检查它；\
             可选超时（秒）。{what}失败就不跑；命令非零退出会如实报告，但{what}保留。"
        ),
        "properties": {
            "command": { "type": "string", "description": "要执行的命令，交给 /bin/sh -c，在会话工作目录里跑" },
            "timeout_secs": { "type": "integer", "minimum": 1, "description": "活动执行时限，秒" }
        },
        "required": ["command"],
        "additionalProperties": false
    })
}

/// 把 `then_run` 接到改动的计划上：第二步是一条普通的 shell 计划（与 `shell` 的 `prepare`
/// 同一个构造），同一来源、同一 Run、同一 ToolCall。带了它，整次调用的恢复方式就是
/// `NoSafeRecovery`。
pub fn attach(
    plan: &mut ExecutionPlan,
    then_run: Option<ThenRun>,
    ctx: &ToolContext,
) -> Result<(), ToolError> {
    let Some(then_run) = then_run else {
        return Ok(());
    };
    let step = plan_for_call(
        &ShellArgs {
            command: then_run.command,
            cwd: None,
            timeout_secs: then_run.timeout_secs,
        },
        ctx,
    )
    .map_err(|error| ToolError::InvalidArguments {
        message: format!("then_run：{error}"),
    })?;
    plan.then_run = Some(Box::new(step));
    plan.recovery = RecoveryMode::NoSafeRecovery;
    plan.validate_steps()
        .map_err(|error| ToolError::InvalidArguments {
            message: error.to_string(),
        })
}

/// 改动 → 核对目标没被动过 → 命令，交回**一条**结果。
///
/// `mutation` 是整份组合计划（改动工具只读它自己的参数与目标），`command` 是
/// [`ApprovedPlan::then_run`] 换出来的第二步。取消与"结果不明"原样往上抛：前者要停下这一轮，
/// 后者要交给人——它们不是"命令失败了"。
pub async fn execute_fused(
    mutator: &dyn Tool,
    mutation: ApprovedPlan,
    shell: &dyn Tool,
    command: ApprovedPlan,
    ctx: &ToolContext,
    sink: &mut dyn OutputWriter,
) -> Result<ToolOutput, ToolError> {
    let target = target_path(mutation.plan())?;

    let mutated = match mutator.execute(mutation, ctx, sink).await {
        Ok(output) if output.status == ToolResultStatus::Completed => output,
        Ok(output) => {
            let text = output.preview.clone().unwrap_or_default();
            return Ok(fused(
                Step::text(text, output.result),
                Then::skipped("改动没有成功，命令没有运行"),
                output.artifacts,
            ));
        }
        Err(error) if passes_through(&error) => return Err(error),
        Err(error) => {
            return Ok(fused(
                Step::error(&error),
                Then::skipped("改动没有成功，命令没有运行"),
                vec![],
            ));
        }
    };

    if let Some(reason) = changed_since(&target, &mutated)? {
        let artifacts = mutated.artifacts.clone();
        return Ok(fused(
            Step::of(mutated),
            Then::skipped(&format!("{reason}，命令没有运行")),
            artifacts,
        ));
    }

    let mut artifacts = mutated.artifacts.clone();
    let then = match shell.execute(command, ctx, sink).await {
        Ok(output) => {
            artifacts.extend(output.artifacts.iter().cloned());
            Then::ran(output)
        }
        Err(error) if passes_through(&error) => return Err(error),
        Err(error) => Then::failed(&error),
    };
    Ok(fused(Step::of(mutated), then, artifacts))
}

/// 不折成"命令失败"的那两种。
fn passes_through(error: &ToolError) -> bool {
    matches!(error, ToolError::Cancelled) || error.is_uncertain()
}

/// 改动之后目标还是不是改动留下的那一份。`Some(理由)` = 不是了（或者说不清）。
fn changed_since(
    target: &std::path::Path,
    mutated: &ToolOutput,
) -> Result<Option<String>, ToolError> {
    let Some(expected) = mutated
        .result
        .get("version")
        .and_then(|version| serde_json::from_value::<FileVersion>(version.clone()).ok())
        .map(|version| version.hash)
    else {
        return Ok(Some(format!(
            "改动没有给出 {} 的结果版本",
            target.display()
        )));
    };
    let now: Option<ContentHash> = current(target)?.map(|(_, version)| version.hash);
    Ok(match now {
        Some(now) if now == expected => None,
        Some(now) => Some(format!(
            "改动之后 {} 又变了（改动留下 {expected}，现在 {now}）",
            target.display()
        )),
        None => Some(format!("改动之后 {} 不见了", target.display())),
    })
}

/// 改动那一步交给结果的东西。
struct Step {
    ok: bool,
    text: String,
    result: serde_json::Value,
}

impl Step {
    fn of(output: ToolOutput) -> Self {
        Self {
            ok: true,
            text: output.preview.unwrap_or_default(),
            result: output.result,
        }
    }

    fn text(text: String, result: serde_json::Value) -> Self {
        Self {
            ok: false,
            text,
            result,
        }
    }

    fn error(error: &ToolError) -> Self {
        Self {
            ok: false,
            text: error.to_string(),
            result: serde_json::json!({ "error": error.to_string() }),
        }
    }
}

/// 命令那一步的结论。
struct Then {
    marker: &'static str,
    status: &'static str,
    text: String,
    result: serde_json::Value,
    exit_code: Option<i32>,
}

impl Then {
    fn skipped(reason: &str) -> Self {
        Self {
            marker: THEN_RUN_SKIPPED,
            status: "skipped",
            text: reason.to_string(),
            result: serde_json::json!({ "reason": reason }),
            exit_code: None,
        }
    }

    fn ran(output: ToolOutput) -> Self {
        let succeeded = output.status == ToolResultStatus::Completed && output.exit_code == Some(0);
        Self {
            marker: if succeeded {
                THEN_RUN_SUCCEEDED
            } else {
                THEN_RUN_FAILED
            },
            status: if succeeded { "succeeded" } else { "failed" },
            text: output.preview.unwrap_or_default(),
            result: output.result,
            exit_code: output.exit_code,
        }
    }

    fn failed(error: &ToolError) -> Self {
        Self {
            marker: THEN_RUN_FAILED,
            status: "failed",
            text: error.to_string(),
            result: serde_json::json!({ "error": error.to_string() }),
            exit_code: None,
        }
    }

    fn succeeded(&self) -> bool {
        self.status == "succeeded"
    }
}

/// 一条结果：正文是改动的那段、标记、命令的那段，按这个顺序——账本那 1 KiB 的预览
/// 截的是开头，改动的结论与标记总在里面；命令的完整输出在这次尝试的 stdout / stderr。
fn fused(
    step: Step,
    then: Then,
    artifacts: Vec<komo_kernel::types::refs::ContentRef>,
) -> ToolOutput {
    let status = if step.ok && then.succeeded() {
        ToolResultStatus::Completed
    } else {
        ToolResultStatus::Failed
    };
    let mut preview = step.text;
    if !preview.is_empty() {
        preview.push('\n');
    }
    preview.push_str(then.marker);
    if !then.text.is_empty() {
        // 跳过的理由跟在标记后面；跑过的命令输出另起一行。
        preview.push(if then.status == "skipped" { ' ' } else { '\n' });
        preview.push_str(&then.text);
    }
    ToolOutput {
        status,
        result: serde_json::json!({
            "mutation": step.result,
            "then_run": { "status": then.status, "result": then.result },
        }),
        exit_code: then.exit_code,
        artifacts,
        preview: Some(preview),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{context, writer};
    use crate::tools::{EditTool, ShellTool, WriteTool};
    use async_trait::async_trait;
    use komo_kernel::types::tool::ToolDefinition;

    fn approve(plan: ExecutionPlan) -> (ApprovedPlan, ApprovedPlan) {
        let approved = ApprovedPlan::new(plan, komo_kernel::test_support::proof());
        let command = approved.then_run().expect("组合计划有第二步");
        (approved, command)
    }

    fn edit_args(command: &str) -> serde_json::Value {
        serde_json::json!({
            "path": "a.txt",
            "match_text": "old",
            "replace_text": "new",
            "then_run": { "command": command },
        })
    }

    #[tokio::test]
    async fn then_run_builds_a_shell_step_and_gives_up_safe_recovery() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
        let ctx = context(dir.path());
        let plan = EditTool::new()
            .prepare(edit_args("cat a.txt"), &ctx)
            .await
            .unwrap();
        let step = plan.then_run.as_deref().expect("带着第二步");
        assert_eq!(step.tool, "shell");
        assert_eq!(step.run, plan.run);
        assert_eq!(step.tool_call, plan.tool_call);
        assert_eq!(step.source, plan.source);
        // 与 `shell` 自己 prepare 出来的是同一个形状。
        let alone = ShellTool::new()
            .prepare(serde_json::json!({ "command": "cat a.txt" }), &ctx)
            .await
            .unwrap();
        assert_eq!(step.args, alone.args);
        assert_eq!(step.versions, alone.versions);
        assert_eq!(plan.recovery, RecoveryMode::NoSafeRecovery);
        // 改动那一步的参数里不再重复一份命令。
        assert!(plan.args.get("then_run").is_none(), "{}", plan.args);
        assert_eq!(plan.validate_steps(), Ok(()));

        // 不带 then_run 的还是原来那份计划。
        let plain = WriteTool::new()
            .prepare(serde_json::json!({ "path": "b.txt", "content": "x" }), &ctx)
            .await
            .unwrap();
        assert!(plain.then_run.is_none());
        assert_eq!(plain.recovery, RecoveryMode::VerifyTarget);
    }

    #[tokio::test]
    async fn an_empty_then_run_command_is_refused_at_prepare_time() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let error = WriteTool::new()
            .prepare(
                serde_json::json!({ "path": "a.txt", "content": "x", "then_run": { "command": " " } }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ToolError::InvalidArguments { message } if message.contains("then_run")),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_mutation_skips_the_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
        let ctx = context(dir.path());
        let marker = dir.path().join("ran");
        let plan = EditTool::new()
            .prepare(edit_args(&format!("touch {}", marker.display())), &ctx)
            .await
            .unwrap();
        // 计划之后、执行之前文件被人改了：改动按版本冲突失败。
        std::fs::write(dir.path().join("a.txt"), "someone else\n").unwrap();
        let (mutation, command) = approve(plan);
        let mut sink = writer(&ctx);

        let output = execute_fused(
            &EditTool::new(),
            mutation,
            &ShellTool::new(),
            command,
            &ctx,
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(output.status, ToolResultStatus::Failed);
        assert!(!marker.exists(), "改动失败，命令不该跑");
        let preview = output.preview.unwrap();
        assert!(preview.contains("版本冲突"), "{preview}");
        assert!(preview.contains(THEN_RUN_SKIPPED), "{preview}");
        assert_eq!(output.result["then_run"]["status"], "skipped");
        assert_eq!(output.exit_code, None);
        assert_eq!(sink.bytes_written(), 0, "命令没跑就没有输出");
    }

    /// 一个改完文件又被"别人"动了一下的 `edit`：模拟改动与命令之间的插队。
    struct MeddledEdit;

    #[async_trait]
    impl Tool for MeddledEdit {
        fn definition(&self) -> ToolDefinition {
            EditTool::new().definition()
        }

        async fn prepare(
            &self,
            args: serde_json::Value,
            ctx: &ToolContext,
        ) -> Result<ExecutionPlan, ToolError> {
            EditTool::new().prepare(args, ctx).await
        }

        async fn execute(
            &self,
            plan: ApprovedPlan,
            ctx: &ToolContext,
            sink: &mut dyn OutputWriter,
        ) -> Result<ToolOutput, ToolError> {
            let path = target_path(plan.plan())?;
            let output = EditTool::new().execute(plan, ctx, sink).await?;
            std::fs::write(&path, "meddled\n").unwrap();
            Ok(output)
        }
    }

    #[tokio::test]
    async fn a_changed_file_skips_the_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
        let ctx = context(dir.path());
        let marker = dir.path().join("ran");
        let plan = MeddledEdit
            .prepare(edit_args(&format!("touch {}", marker.display())), &ctx)
            .await
            .unwrap();
        let (mutation, command) = approve(plan);

        let output = execute_fused(
            &MeddledEdit,
            mutation,
            &ShellTool::new(),
            command,
            &ctx,
            &mut writer(&ctx),
        )
        .await
        .unwrap();

        assert!(!marker.exists(), "目标被动过，命令不该跑");
        assert_eq!(output.status, ToolResultStatus::Failed);
        let preview = output.preview.unwrap();
        assert!(preview.contains("替换了 1 处"), "改动的结论还在：{preview}");
        assert!(preview.contains(THEN_RUN_SKIPPED), "{preview}");
        assert!(preview.contains("又变了"), "{preview}");
        assert_eq!(output.result["mutation"]["replacements"], 1);
        assert_eq!(output.result["then_run"]["status"], "skipped");
    }

    #[tokio::test]
    async fn a_nonzero_exit_reports_failure_and_keeps_the_edit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
        let ctx = context(dir.path());
        let plan = EditTool::new()
            .prepare(edit_args("echo boom >&2; exit 3"), &ctx)
            .await
            .unwrap();
        let (mutation, command) = approve(plan);
        let mut sink = writer(&ctx);

        let output = execute_fused(
            &EditTool::new(),
            mutation,
            &ShellTool::new(),
            command,
            &ctx,
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "new\n",
            "命令失败不回滚改动"
        );
        assert_eq!(output.status, ToolResultStatus::Failed);
        assert_eq!(output.exit_code, Some(3));
        assert_eq!(output.result["then_run"]["status"], "failed");
        assert_eq!(output.result["then_run"]["result"]["exit_code"], 3);
        let preview = output.preview.unwrap();
        assert!(preview.contains(THEN_RUN_FAILED), "{preview}");
        assert!(preview.contains("boom"), "{preview}");
    }

    #[tokio::test]
    async fn a_successful_fused_write_streams_the_command_into_the_same_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let plan = WriteTool::new()
            .prepare(
                serde_json::json!({
                    "path": "hello.sh",
                    "content": "echo hello from the file\n",
                    "then_run": { "command": "sh hello.sh" },
                }),
                &ctx,
            )
            .await
            .unwrap();
        let (mutation, command) = approve(plan);
        let mut sink = writer(&ctx);

        let output = execute_fused(
            &WriteTool::new(),
            mutation,
            &ShellTool::new(),
            command,
            &ctx,
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(output.status, ToolResultStatus::Completed);
        assert_eq!(output.exit_code, Some(0));
        let preview = output.preview.unwrap();
        let (head, tail) = preview.split_once(THEN_RUN_SUCCEEDED).expect("有标记");
        assert!(head.contains("已创建"), "改动的结论在前：{preview}");
        assert!(tail.contains("hello from the file"), "{preview}");
        assert_eq!(output.result["mutation"]["created"], true);
        assert_eq!(output.result["then_run"]["status"], "succeeded");
        assert!(sink.bytes_written() > 0, "命令输出流进了这次尝试的写入器");
    }
}
