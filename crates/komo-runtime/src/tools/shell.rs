//! `shell`：命令、工作目录、超时；返回退出码和输出。**管理进程组、限制输出、支持
//! 取消**（§4）。
//!
//! 取消不是"不再等它"：Tokio 不会因为 handle 被丢掉就结束进程，而 `Child::kill` 只
//! 杀直接子进程——`sh -c 'sleep 30 &'` 的孙子进程会留在系统里。所以命令跑在**自己的
//! 进程组**里，取消时信号发给整个组，然后等回收（实现在 [`super::process`]）。
//!
//! 恢复方式是 [`RecoveryMode::NoSafeRecovery`]：任意命令不能仅凭名称被判定安全
//! （§8.6）。停在 `waiting + intervention` 由人接手，好过把 `deploy.sh` 再跑一遍。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{
    ApprovedPlan, ExecutionPlan, Operation, PlanVersions, RecoveryMode,
};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::process::{ChildRegistration, ChildSpec, ProcessError, run_child};
use super::{normalized, parse_args, plan_time};

/// 一条命令默认跑多久。
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// 一次执行默认最多往输出存储里写多少字节。
pub const DEFAULT_OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;

/// 交给子进程的**明确**环境变量。继承整个环境等于把 API key 交给任意脚本（§5.3）。
const INHERITED: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "TZ", "USER", "TMPDIR"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellArgs {
    pub command: String,
    /// 工作目录；相对路径按上下文的 cwd 解析。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellResult {
    pub command: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub timed_out: bool,
    /// 输出超过预算，完整正文里也只有截到的那部分。
    #[serde(default)]
    pub output_truncated: bool,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    /// 尾部片段。完整 stdout / stderr 在这次尝试的输出目录里。
    pub stdout_tail: String,
    pub stderr_tail: String,
}

pub struct ShellTool {
    default_timeout: Duration,
    output_limit: u64,
    shell: String,
    /// 在册登记（§8.7）。`None` = 不登记。
    register: Option<ChildRegistration>,
}

impl std::fmt::Debug for ShellTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellTool")
            .field("shell", &self.shell)
            .field("default_timeout", &self.default_timeout)
            .finish_non_exhaustive()
    }
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellTool {
    pub fn new() -> Self {
        Self {
            default_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            output_limit: DEFAULT_OUTPUT_LIMIT,
            shell: "/bin/sh".into(),
            register: None,
        }
    }

    /// 把这个工具起的子进程登记在册，恢复扫描才核实得了它们（§8.7）。
    pub fn registered(mut self, registration: ChildRegistration) -> Self {
        self.register = Some(registration);
        self
    }

    pub fn with_limits(mut self, timeout: Duration, output_limit: u64) -> Self {
        self.default_timeout = timeout;
        self.output_limit = output_limit;
        self
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell".into(),
            description:
                "在自己的进程组里跑一条 shell 命令，返回退出码与输出。搜索、git、构建、测试都走它。"
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "要执行的命令，交给 /bin/sh -c" },
                    "cwd": { "type": "string", "description": "工作目录；相对路径按会话工作目录解析" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "description": "活动执行时限，秒" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        let args: ShellArgs = parse_args(args, "shell")?;
        if args.command.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                message: "command 不能是空串".into(),
            });
        }
        let cwd = match &args.cwd {
            Some(raw) => super::paths::resolve(raw, &ctx.cwd)?,
            None => ctx.cwd.clone(),
        };
        Ok(ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: "shell".into(),
            operation: Operation::ShellCommand {
                command: args.command.clone(),
            },
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&args)?,
            cwd: Some(cwd),
            // 命令会碰哪些文件，在跑起来之前不知道——所以不编造目标。规则按
            // `ShellCommand` 与命令正文匹配，不按路径。
            targets: vec![],
            versions: PlanVersions {
                // 命令正文的快照：命令改一个字节，绑定它的授权就覆盖不到了（§5.4）。
                code: Some(ContentHash::of_str(&args.command)),
                ..Default::default()
            },
            resources: vec![],
            recovery: RecoveryMode::NoSafeRecovery,
        })
    }

    async fn execute(
        &self,
        plan: ApprovedPlan,
        ctx: &ToolContext,
        sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let plan = plan.plan();
        let args: ShellArgs = parse_args(plan.args.clone(), "shell")?;
        let cwd: PathBuf = plan.cwd.clone().unwrap_or_else(|| ctx.cwd.clone());
        let timeout = args
            .timeout_secs
            .map_or(self.default_timeout, Duration::from_secs);

        let spec = ChildSpec {
            program: self.shell.clone(),
            args: vec!["-c".into(), args.command.clone()],
            cwd: cwd.clone(),
            env: environment(ctx),
            stdin: None,
            timeout,
            output_limit: self.output_limit,
            register: self.register.clone(),
            label: format!("shell · run {} · call {}", ctx.run, ctx.call),
        };

        let outcome = run_child(spec, sink, &ctx.cancel)
            .await
            .map_err(|error| match error {
                ProcessError::Spawn(message) => ToolError::Failed {
                    message: format!("起不来：{message}"),
                },
                ProcessError::Io(message) | ProcessError::Sink(message) => {
                    ToolError::Failed { message }
                }
            })?;

        if outcome.cancelled {
            return Err(ToolError::Cancelled);
        }
        if outcome.timed_out {
            return Err(ToolError::Timeout {
                after_secs: timeout.as_secs(),
            });
        }

        let result = ShellResult {
            command: args.command,
            cwd: cwd.display().to_string(),
            exit_code: outcome.exit_code,
            timed_out: false,
            output_truncated: outcome.truncated,
            stdout_bytes: outcome.stdout_bytes,
            stderr_bytes: outcome.stderr_bytes,
            stdout_tail: outcome.stdout_tail,
            stderr_tail: outcome.stderr_tail,
        };
        // 非零退出码是**结果**，不是工具失败：模型看得到退出码才改得动命令（§6）。
        let status = if outcome.exit_code == Some(0) {
            ToolResultStatus::Completed
        } else {
            ToolResultStatus::Failed
        };
        let preview = format!(
            "exit={} stdout={}B stderr={}B{}\n{}{}",
            result
                .exit_code
                .map_or_else(|| "?".to_string(), |code| code.to_string()),
            result.stdout_bytes,
            result.stderr_bytes,
            if result.output_truncated {
                "（输出已截断）"
            } else {
                ""
            },
            result.stdout_tail,
            result.stderr_tail
        );
        Ok(ToolOutput {
            status,
            result: serde_json::to_value(&result).map_err(|error| ToolError::Failed {
                message: error.to_string(),
            })?,
            exit_code: result.exit_code,
            artifacts: vec![],
            preview: Some(preview),
        })
    }

    // `verify` 用默认实现：任意命令没有可用的核对方式，落到 waiting + intervention（§8.6）。
}

fn environment(ctx: &ToolContext) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = INHERITED
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| ((*name).to_string(), value))
        })
        .collect();
    env.entry("PATH".into())
        .or_insert_with(|| "/usr/local/bin:/usr/bin:/bin".into());
    // 非交互、无颜色：给管道读的输出不该带控制序列。
    env.insert("TERM".into(), "dumb".into());
    env.insert("KOMO_SESSION".into(), ctx.session.to_string());
    env.insert("KOMO_RUN".into(), ctx.run.to_string());
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::process::process_group_alive;
    use crate::tools::test_support::{approved, context, context_with_cancel, writer};
    use komo_kernel::types::tool::CancelToken;

    fn shell_result(output: &ToolOutput) -> ShellResult {
        serde_json::from_value(output.result.clone()).expect("shell 的结果")
    }

    #[tokio::test]
    async fn it_returns_the_exit_code_and_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = ShellTool::new();
        let mut sink = writer(&ctx);
        let plan = tool
            .prepare(serde_json::json!({ "command": "echo hi" }), &ctx)
            .await
            .unwrap();
        assert_eq!(
            plan.operation,
            Operation::ShellCommand {
                command: "echo hi".into()
            }
        );
        assert_eq!(plan.recovery, RecoveryMode::NoSafeRecovery);
        assert_eq!(plan.versions.code, Some(ContentHash::of_str("echo hi")));

        let output = tool.execute(approved(plan), &ctx, &mut sink).await.unwrap();
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(shell_result(&output).stdout_tail.trim(), "hi");
        assert!(sink.bytes_written() > 0, "输出流进了写入器");
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_a_result_the_model_can_read_not_a_tool_failure() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = ShellTool::new();
        let mut sink = writer(&ctx);
        let plan = tool
            .prepare(serde_json::json!({ "command": "exit 3" }), &ctx)
            .await
            .unwrap();
        let output = tool.execute(approved(plan), &ctx, &mut sink).await.unwrap();
        assert_eq!(output.status, ToolResultStatus::Failed);
        assert_eq!(output.exit_code, Some(3));
    }

    #[tokio::test]
    async fn the_command_runs_in_the_directory_the_plan_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let ctx = context(dir.path());
        let tool = ShellTool::new();
        let mut sink = writer(&ctx);
        let plan = tool
            .prepare(serde_json::json!({ "command": "pwd", "cwd": "sub" }), &ctx)
            .await
            .unwrap();
        let output = tool.execute(approved(plan), &ctx, &mut sink).await.unwrap();
        assert!(
            shell_result(&output).stdout_tail.trim().ends_with("sub"),
            "{:?}",
            shell_result(&output).stdout_tail
        );
    }

    #[tokio::test]
    async fn the_environment_is_an_explicit_set_not_whatever_the_gateway_had() {
        // SAFETY: 测试进程里设一个变量，紧接着读回来；没有别的线程依赖它。
        unsafe { std::env::set_var("KOMO_TEST_SECRET", "must-not-leak") };
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = ShellTool::new();
        let mut sink = writer(&ctx);
        let plan = tool
            .prepare(
                serde_json::json!({ "command": "echo \"[${KOMO_TEST_SECRET}]\"" }),
                &ctx,
            )
            .await
            .unwrap();
        let output = tool.execute(approved(plan), &ctx, &mut sink).await.unwrap();
        assert_eq!(shell_result(&output).stdout_tail.trim(), "[]");
        unsafe { std::env::remove_var("KOMO_TEST_SECRET") };
    }

    #[tokio::test]
    async fn cancelling_stops_the_child_and_its_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancelToken::new();
        let ctx = context_with_cancel(dir.path(), cancel.clone());
        let tool = ShellTool::new();
        let mut sink = writer(&ctx);
        let pid_file = dir.path().join("pgid");
        let plan = tool
            .prepare(
                serde_json::json!({
                    "command": format!("echo $$ > {}; sleep 30 & wait", pid_file.display())
                }),
                &ctx,
            )
            .await
            .unwrap();

        let stopper = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            stopper.cancel();
        });
        let error = tool
            .execute(approved(plan), &ctx, &mut sink)
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Cancelled), "{error:?}");

        let pgid: u32 = std::fs::read_to_string(&pid_file)
            .expect("子进程写下了自己的 pid")
            .trim()
            .parse()
            .unwrap();
        // 进程组是自己的（process_group(0)），所以 pid 就是 pgid。
        for _ in 0..50 {
            if !process_group_alive(pgid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("进程组 {pgid} 还活着——取消没有回收它");
    }

    #[tokio::test]
    async fn a_timeout_is_reported_as_a_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = ShellTool::new();
        let mut sink = writer(&ctx);
        let plan = tool
            .prepare(
                serde_json::json!({ "command": "sleep 30", "timeout_secs": 1 }),
                &ctx,
            )
            .await
            .unwrap();
        let error = tool
            .execute(approved(plan), &ctx, &mut sink)
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Timeout { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn an_empty_command_is_refused_at_prepare_time() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = ShellTool::new();
        let error = tool
            .prepare(serde_json::json!({ "command": "   " }), &ctx)
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::InvalidArguments { .. }),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn an_arbitrary_command_has_no_verification_so_it_lands_on_a_human() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = ShellTool::new();
        let plan = tool
            .prepare(serde_json::json!({ "command": "deploy.sh" }), &ctx)
            .await
            .unwrap();
        assert_eq!(
            tool.verify(&plan, &ctx).await.unwrap(),
            komo_kernel::types::plan::Verification::Unavailable
        );
    }
}
