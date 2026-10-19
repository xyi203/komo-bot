//! `bash`：命令、工作目录、超时；返回退出码和输出。**管理进程组、完整落盘、支持
//! 取消**（§4）。
//!
//! 取消不是"不再等它"：Tokio 不会因为 handle 被丢掉就结束进程，而 `Child::kill` 只
//! 杀直接子进程——`sh -c 'sleep 30 &'` 的孙子进程会留在系统里。所以命令跑在**自己的
//! 进程组**里，取消时信号发给整个组，然后等回收（实现在 [`super::process`]）。
//!
//! 恢复方式是 [`RecoveryMode::NoSafeRecovery`]：任意命令不能仅凭名称被判定安全
//! （§8.6）。重启后不重跑，中断结果连同已落盘的部分输出交给模型——好过把 `deploy.sh`
//! 再跑一遍。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::OperationId;
#[cfg(test)]
use komo_kernel::types::plan::Replay;
use komo_kernel::types::plan::{ExecutionPlan, Operation, PlanSource, PlanVersions, RecoveryMode};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::process::{ChildRegistration, ChildSpec, ProcessError, run_child};
use super::protect::Protected;
use super::{normalized, parse_args, plan_time};

/// 一条命令默认跑多久。
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// 默认把输出完整写进已有的 `tool-output`；模型视图的 50 KiB / 2000 行限制在 kernel
/// 投影里执行。测试和嵌入方仍可通过 `with_limits` 明确设置存储上限。
pub const DEFAULT_OUTPUT_LIMIT: u64 = u64::MAX;

/// 交给子进程的**明确**环境变量。继承整个环境等于把 API key 交给任意脚本。
const INHERITED: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "TZ", "USER", "TMPDIR"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BashArgs {
    pub command: String,
    /// 工作目录；相对路径按上下文的 cwd 解析。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BashResult {
    pub command: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub timed_out: bool,
    /// 调用方明确设置了存储上限且输出超过预算；默认不会截断落盘正文。
    #[serde(default)]
    pub output_truncated: bool,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    /// 首尾摘录。完整 stdout / stderr 在这次尝试的输出目录里。
    pub stdout_excerpt: String,
    pub stderr_excerpt: String,
}

/// 一条 shell 命令的执行计划**形状**：工具名、操作、命令正文的版本快照、恢复方式。
///
/// **唯一的构造处**：[`BashTool::prepare`] 与 `then_run`（经 [`plan_for_call`]）都调它
/// ——手写第二份迟早会在几处之间长出一条缝（§10）。`run` / `tool_call` 不在这里填：那两个
/// 字段来自一次具体的调用。
pub fn plan_for(source: PlanSource, cwd: PathBuf, command: &str) -> ExecutionPlan {
    ExecutionPlan {
        operation_id: OperationId::new_at(plan_time()),
        source,
        tool: "bash".into(),
        operation: Operation::ShellCommand {
            command: command.to_string(),
        },
        run: None,
        tool_call: None,
        args: serde_json::json!({ "command": command }),
        cwd: Some(cwd),
        // 命令会碰哪些文件，在跑起来之前不知道——所以不编造目标。规则按
        // `ShellCommand` 与命令正文匹配，不按路径。
        targets: vec![],
        versions: PlanVersions {
            // 命令正文的快照：命令改一个字节，计划就是另一份（§7.2）。
            code: Some(ContentHash::of_str(command)),
            ..Default::default()
        },
        recovery: RecoveryMode::NoSafeRecovery,
        then_run: None,
    }
}

/// 一次具体调用的 shell 计划：[`plan_for`] 加上这次调用的 Run / ToolCall 与完整参数。
///
/// `bash` 自己的 `prepare` 与 `edit` / `write` 的 `then_run`（[`super::fusion`]）共用它：
/// 第二步就是一条普通的 shell 计划，账本里与单独一次 `bash` 同一个形状。
pub fn plan_for_call(args: &BashArgs, ctx: &ToolContext) -> Result<ExecutionPlan, ToolError> {
    if args.command.trim().is_empty() {
        return Err(ToolError::InvalidArguments {
            message: "command 不能是空串".into(),
        });
    }
    let cwd = match &args.cwd {
        Some(raw) => super::paths::resolve(raw, &ctx.cwd)?,
        None => ctx.cwd.clone(),
    };
    let mut plan = plan_for(ctx.source.clone(), cwd, &args.command);
    plan.run = Some(ctx.run.clone());
    plan.tool_call = Some(ctx.call.clone());
    // 完整的原始参数（可能带 `cwd` / `timeout_secs`），不是 `plan_for` 那份精简的。
    plan.args = normalized(args)?;
    Ok(plan)
}

pub struct BashTool {
    default_timeout: Duration,
    output_limit: u64,
    shell: String,
    /// 在册登记（§8.7）。`None` = 不登记。
    register: Option<ChildRegistration>,
    /// 数据目录只读挂载。`None` = 直接跑（没给名单，或这台机器上 bwrap 用不了）。
    sandbox: Option<Sandbox>,
}

/// 用 bwrap 把 komo 自己的状态只读挂上再跑命令（§7 的硬限制延伸到 `bash`）。
///
/// 只防手滑，不防恶意：整棵树照旧可写（`--dev-bind / /`），只是名单里的路径换成只读挂载；
/// 名单里还不存在的文件（比如重启前的 `state.db-wal`）挂不上，把数据目录整个改名也拦不住。
/// bwrap 会设 no_new_privs，命令里的 `sudo` / setuid 程序因此跑不了。
#[derive(Debug, Clone)]
struct Sandbox {
    bwrap: PathBuf,
    protected: Protected,
}

impl Sandbox {
    /// 找得到 bwrap、而且在这台机器上真起得来（内核可能关了非特权 user namespace）才用。
    fn probe(protected: Protected) -> Option<Self> {
        if protected.is_empty() {
            return None;
        }
        let bwrap = ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())?;
        let works = std::process::Command::new(&bwrap)
            .args(["--dev-bind", "/", "/", "--", "/bin/sh", "-c", "true"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !works {
            tracing::warn!(bwrap = %bwrap.display(), "bwrap 起不来，shell 不挂数据目录保护");
            return None;
        }
        Some(Self { bwrap, protected })
    }

    /// `bwrap … -- <shell> -c <command>` 的程序与参数。
    fn wrap(&self, shell: &str, command: &str) -> (String, Vec<String>) {
        let mut args: Vec<String> = vec!["--dev-bind".into(), "/".into(), "/".into()];
        for path in self.protected.existing() {
            let path = path.display().to_string();
            args.extend(["--ro-bind".into(), path.clone(), path]);
        }
        args.extend([
            "--die-with-parent".into(),
            "--".into(),
            shell.into(),
            "-c".into(),
            command.into(),
        ]);
        (self.bwrap.display().to_string(), args)
    }
}

/// 有 bash 用 bash：模型写的命令默认是 bash 的写法（`[[ ]]`、`<(…)`、数组），`/bin/sh`
/// 在 Debian 系是 dash，这些都跑不了。
fn default_shell() -> String {
    ["/bin/bash", "/usr/bin/bash"]
        .into_iter()
        .find(|path| std::path::Path::new(path).is_file())
        .unwrap_or("/bin/sh")
        .into()
}

impl std::fmt::Debug for BashTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BashTool")
            .field("shell", &self.shell)
            .field("default_timeout", &self.default_timeout)
            .field("sandbox", &self.sandbox)
            .finish_non_exhaustive()
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

impl BashTool {
    pub fn new() -> Self {
        Self {
            default_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            output_limit: DEFAULT_OUTPUT_LIMIT,
            shell: default_shell(),
            register: None,
            sandbox: None,
        }
    }

    /// 把这个工具起的子进程登记在册，恢复扫描才核实得了它们（§8.7）。
    pub fn registered(mut self, registration: ChildRegistration) -> Self {
        self.register = Some(registration);
        self
    }

    /// 把这些路径只读挂上再跑命令；bwrap 用不了时照旧直接跑（启动时探一次）。
    pub fn protecting(mut self, paths: Vec<PathBuf>) -> Self {
        self.sandbox = Sandbox::probe(Protected::new(paths));
        self
    }

    pub fn with_limits(mut self, timeout: Duration, output_limit: u64) -> Self {
        self.default_timeout = timeout;
        self.output_limit = output_limit;
        self
    }

    /// 按 `bash` 自己的规矩（bash、数据目录只读挂载）起一条命令要的 [`ChildSpec`]。
    ///
    /// 不登记在册、不限输出：给 `monitor` 用（`docs/monitor.md`），它的进程不属于任何
    /// 一次调用，在册与收尾由它自己管。
    pub fn child_spec(
        &self,
        command: &str,
        cwd: PathBuf,
        env: BTreeMap<String, String>,
        timeout: Duration,
        label: String,
    ) -> ChildSpec {
        let (program, args) = match &self.sandbox {
            Some(sandbox) => sandbox.wrap(&self.shell, command),
            None => (self.shell.clone(), vec!["-c".into(), command.to_string()]),
        };
        ChildSpec {
            program,
            args,
            cwd,
            env,
            timeout,
            output_limit: u64::MAX,
            register: None,
            label,
        }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".into(),
            description:
                "在自己的进程组里跑一条 bash 命令，返回退出码与输出。默认就在会话工作目录里跑，\
                 不用先 cd 过去；要换目录给 cwd。git、构建、测试、跑程序走它；\
                 只是看文件、搜代码、列文件用 read / rg（列文件设 list_files=true），\
                 要读要搜好几处用 codemode。\
                 等一件慢事（下载、构建、部署）就在一条命令里循环轮询、每轮打印一行进度，并把 \
                 timeout_secs 给够：release 构建、整个工作区的测试常要 5–15 分钟，给 900 以上，\
                 别用默认的 120；到点会终止进程组，到点前打印的输出照样交回，接着再发一条即可。\
                 不要每轮只 sleep 一下就交回来再问一次。\
                 命令跑着的时候，用户说的话要等它跑完才轮得到：在跟用户对话的会话里，预计要等\
                 十分钟以上的事别在这里等——要接着做一串事、最后给结论的派 task，只是等一个结果\
                 或盯一个状态的挂 monitor（脚本只在完成或出错时打一行），这一轮先回复用户。\
                 你自己就是后台任务（没有 task 可用）时照上面在这里等，别挂 monitor：任务这一轮\
                 一结束，结果就交回去了。"
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "要执行的命令，交给 bash -c（没有 bash 的机器上是 /bin/sh -c）" },
                    "cwd": { "type": "string", "description": "工作目录；相对路径按会话工作目录解析" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "description": "活动执行时限，秒；省略 = 120，上限是这次调用的活动执行时限" }
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
        let args: BashArgs = parse_args(args, "bash")?;
        plan_for_call(&args, ctx)
    }

    async fn execute(
        &self,
        plan: ExecutionPlan,
        ctx: &ToolContext,
        sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let plan = &plan;
        let args: BashArgs = parse_args(plan.args.clone(), "bash")?;
        let cwd: PathBuf = plan.cwd.clone().unwrap_or_else(|| ctx.cwd.clone());
        let timeout = args
            .timeout_secs
            .map_or(self.default_timeout, Duration::from_secs)
            .min(ctx.call_timeout);

        let (program, child_args) = match &self.sandbox {
            Some(sandbox) => sandbox.wrap(&self.shell, &args.command),
            None => (self.shell.clone(), vec!["-c".into(), args.command.clone()]),
        };
        let spec = ChildSpec {
            program,
            args: child_args,
            cwd: cwd.clone(),
            env: environment(ctx),
            timeout,
            output_limit: self.output_limit,
            register: self.register.clone(),
            label: format!("bash · run {} · call {}", ctx.run, ctx.call),
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

        // 超时**不是一句光秃秃的"超时"**：到点前打印的输出照样交回（与 `rg` 同一条，§4）。
        // 轮询循环跑满时限时，模型要看得到最后一次查到的进度，才知道接着等还是换个做法；
        // 只给一句"超时：600s"，它只能把同一个循环从头再跑一遍。状态是失败——命令没跑完。
        let result = BashResult {
            command: args.command,
            cwd: cwd.display().to_string(),
            exit_code: outcome.exit_code,
            timed_out: outcome.timed_out,
            output_truncated: outcome.truncated,
            stdout_bytes: outcome.stdout_bytes,
            stderr_bytes: outcome.stderr_bytes,
            stdout_excerpt: outcome.stdout_excerpt,
            stderr_excerpt: outcome.stderr_excerpt,
        };
        // 非零退出码是**结果**，不是工具失败：模型看得到退出码才改得动命令（§6）。
        let status = if outcome.exit_code == Some(0) && !outcome.timed_out {
            ToolResultStatus::Completed
        } else {
            ToolResultStatus::Failed
        };
        let head = if outcome.timed_out {
            format!(
                "timeout={}s（进程组已终止，下面是到点前的输出）",
                timeout.as_secs()
            )
        } else {
            format!(
                "exit={}",
                result
                    .exit_code
                    .map_or_else(|| "?".to_string(), |code| code.to_string())
            )
        };
        let preview = format!(
            "{head} stdout={}B stderr={}B{}\n{}{}",
            result.stdout_bytes,
            result.stderr_bytes,
            if result.output_truncated {
                "（输出已截断）"
            } else {
                ""
            },
            result.stdout_excerpt,
            result.stderr_excerpt
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

    // `replay` 用默认的 unsafe：任意命令重启后不重跑（§8.6）。
}

fn environment(ctx: &ToolContext) -> BTreeMap<String, String> {
    let mut env = base_environment();
    env.insert("KOMO_SESSION".into(), ctx.session.to_string());
    env.insert("KOMO_RUN".into(), ctx.run.to_string());
    env
}

/// 交给子进程的明确环境：继承的那几个变量 + 非交互终端。不带会话与 Run。
pub fn base_environment() -> BTreeMap<String, String> {
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
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::process::process_group_alive;
    use crate::tools::test_support::{context, context_with_cancel, writer};
    use komo_kernel::types::tool::CancelToken;

    /// 列文件也指到 rg 的 list_files，不在 shell 里跑 ls / find。
    #[test]
    fn shell_description_routes_file_listing_to_rg() {
        use komo_kernel::traits::Tool;
        let description = BashTool::new().definition().description;
        assert!(description.contains("列文件用 read / rg"), "{description}");
        assert!(description.contains("list_files=true"), "{description}");
    }

    fn shell_result(output: &ToolOutput) -> BashResult {
        serde_json::from_value(output.result.clone()).expect("bash 的结果")
    }

    #[tokio::test]
    async fn the_state_is_read_only_under_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir(home.join("sessions")).unwrap();
        for name in ["state.db", "state.db-log", "sessions/events.jsonl"] {
            std::fs::write(home.join(name), "keep").unwrap();
        }
        let tool = BashTool::new().protecting(vec![home.join("sessions"), home.join("state.db")]);
        if tool.sandbox.is_none() {
            eprintln!("这台机器上 bwrap 用不了，跳过");
            return;
        }
        let ctx = context(home);
        let mut sink = writer(&ctx);
        let command = "rm -f state.db-log; echo x > state.db; rm -rf sessions/*; echo ok > other";
        let plan = tool
            .prepare(serde_json::json!({ "command": command }), &ctx)
            .await
            .unwrap();
        tool.execute(plan, &ctx, &mut sink).await.unwrap();
        for name in ["state.db", "state.db-log", "sessions/events.jsonl"] {
            assert_eq!(
                std::fs::read_to_string(home.join(name)).unwrap(),
                "keep",
                "{name}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(home.join("other")).unwrap().trim(),
            "ok"
        );
    }

    #[tokio::test]
    async fn commands_run_under_bash_when_there_is_one() {
        if !std::path::Path::new("/bin/bash").is_file() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = BashTool::new();
        let mut sink = writer(&ctx);
        let plan = tool
            .prepare(
                serde_json::json!({ "command": "[[ -n $BASH_VERSION ]] && echo bash" }),
                &ctx,
            )
            .await
            .unwrap();
        let output = tool.execute(plan, &ctx, &mut sink).await.unwrap();
        assert_eq!(shell_result(&output).stdout_excerpt.trim(), "bash");
    }

    #[tokio::test]
    async fn it_returns_the_exit_code_and_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = BashTool::new();
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

        let output = tool.execute(plan, &ctx, &mut sink).await.unwrap();
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(shell_result(&output).stdout_excerpt.trim(), "hi");
        assert!(sink.bytes_written() > 0, "输出流进了写入器");
    }

    #[tokio::test]
    async fn default_shell_output_is_not_cut_before_it_reaches_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = BashTool::new();
        let mut sink = writer(&ctx);
        let bytes = 8 * 1024 * 1024 + 1;
        let plan = tool
            .prepare(
                serde_json::json!({
                    "command": format!("yes x | head -c {bytes}")
                }),
                &ctx,
            )
            .await
            .unwrap();

        let output = tool.execute(plan, &ctx, &mut sink).await.unwrap();
        assert_eq!(sink.bytes_written(), bytes);
        assert!(!shell_result(&output).output_truncated);
        let preview = output.preview.expect("有模型预览");
        assert!(preview.len() >= 50 * 1024, "投影前不能只剩旧的 7 KiB");
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_a_result_the_model_can_read_not_a_tool_failure() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = BashTool::new();
        let mut sink = writer(&ctx);
        let plan = tool
            .prepare(serde_json::json!({ "command": "exit 3" }), &ctx)
            .await
            .unwrap();
        let output = tool.execute(plan, &ctx, &mut sink).await.unwrap();
        assert_eq!(output.status, ToolResultStatus::Failed);
        assert_eq!(output.exit_code, Some(3));
    }

    #[tokio::test]
    async fn the_command_runs_in_the_directory_the_plan_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let ctx = context(dir.path());
        let tool = BashTool::new();
        let mut sink = writer(&ctx);
        let plan = tool
            .prepare(serde_json::json!({ "command": "pwd", "cwd": "sub" }), &ctx)
            .await
            .unwrap();
        let output = tool.execute(plan, &ctx, &mut sink).await.unwrap();
        assert!(
            shell_result(&output).stdout_excerpt.trim().ends_with("sub"),
            "{:?}",
            shell_result(&output).stdout_excerpt
        );
    }

    #[tokio::test]
    async fn the_environment_is_an_explicit_set_not_whatever_the_gateway_had() {
        // SAFETY: 测试进程里设一个变量，紧接着读回来；没有别的线程依赖它。
        unsafe { std::env::set_var("KOMO_TEST_SECRET", "must-not-leak") };
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = BashTool::new();
        let mut sink = writer(&ctx);
        let plan = tool
            .prepare(
                serde_json::json!({ "command": "echo \"[${KOMO_TEST_SECRET}]\"" }),
                &ctx,
            )
            .await
            .unwrap();
        let output = tool.execute(plan, &ctx, &mut sink).await.unwrap();
        assert_eq!(shell_result(&output).stdout_excerpt.trim(), "[]");
        unsafe { std::env::remove_var("KOMO_TEST_SECRET") };
    }

    #[tokio::test]
    async fn cancelling_stops_the_child_and_its_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancelToken::new();
        let ctx = context_with_cancel(dir.path(), cancel.clone());
        let tool = BashTool::new();
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

        // 等子进程写下 pid 再取消：固定睡一段的话，负载下取消会赶在 `echo $$` 之前。
        let stopper = cancel.clone();
        let written = pid_file.clone();
        tokio::spawn(async move {
            while !std::fs::read_to_string(&written).is_ok_and(|text| text.ends_with('\n')) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            stopper.cancel();
        });
        let error = tool.execute(plan, &ctx, &mut sink).await.unwrap_err();
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
    async fn a_timeout_hands_back_what_was_printed_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = BashTool::new();
        let mut sink = writer(&ctx);
        let plan = tool
            .prepare(
                serde_json::json!({
                    "command": "echo 'poll 1: 3/23 layers'; sleep 30",
                    "timeout_secs": 1
                }),
                &ctx,
            )
            .await
            .unwrap();
        let output = tool.execute(plan, &ctx, &mut sink).await.unwrap();
        assert_eq!(output.status, ToolResultStatus::Failed);
        assert_eq!(output.result["timed_out"], true);
        let preview = output.preview.unwrap();
        assert!(preview.starts_with("timeout=1s"), "{preview}");
        assert!(
            preview.contains("poll 1: 3/23 layers"),
            "到点前打印的进度要交回模型：{preview}"
        );
    }

    /// 模型要的时限比活动执行时限长：收紧到上限，由 shell 自己杀进程报超时——否则执行器
    /// 先到点，一条跑了一半的命令就只能报"结果不明"。
    #[tokio::test]
    async fn a_requested_timeout_beyond_the_call_limit_is_clamped_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = context(dir.path());
        ctx.call_timeout = Duration::from_secs(1);
        let tool = BashTool::new();
        let mut sink = writer(&ctx);
        let plan = tool
            .prepare(
                serde_json::json!({ "command": "sleep 30", "timeout_secs": 1800 }),
                &ctx,
            )
            .await
            .unwrap();
        let output = tool.execute(plan, &ctx, &mut sink).await.unwrap();
        assert_eq!(output.result["timed_out"], true);
        assert!(
            output.preview.unwrap().starts_with("timeout=1s"),
            "收紧到活动执行时限 1s"
        );
    }

    #[tokio::test]
    async fn an_empty_command_is_refused_at_prepare_time() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = BashTool::new();
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
    async fn an_arbitrary_command_is_never_replayed_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = BashTool::new();
        let plan = tool
            .prepare(serde_json::json!({ "command": "deploy.sh" }), &ctx)
            .await
            .unwrap();
        assert_eq!(plan.recovery.replay(), Replay::Unsafe);
        assert_eq!(tool.replay(), Replay::Unsafe);
    }
}
