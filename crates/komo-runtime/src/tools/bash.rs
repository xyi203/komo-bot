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
use std::sync::Arc;
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

use super::detach::{DetachSlot, DetachedCall, long_sleep, tail_lines};
use super::process::{ChildRegistration, ChildSpec, Driven, ProcessError, run_child_detachable};
use super::protect::Protected;
use super::{normalized, parse_args, plan_time};

/// 一条命令默认跑多久。
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// 默认把输出完整写进已有的 `tool-output`；模型视图的 30 KiB / 2000 行限制在 kernel
/// 投影里执行。测试和嵌入方仍可通过 `with_limits` 明确设置存储上限。
pub const DEFAULT_OUTPUT_LIMIT: u64 = u64::MAX;
/// 用户插话后，前台命令至少跑这么久才提前转后台：几秒就完的命令照常跑完。
const INTERJECT_GRACE: Duration = Duration::from_secs(3);

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
    /// 命令转到了后台，值是它的 id（和 `monitor` 同一个 id 空间）。这次调用没有退出码。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached: Option<String>,
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
    /// Gateway 的数据目录，作为 `KOMO_HOME` 交给子进程（附录 C.3.3）。`None` = 不带。
    komo_home: Option<PathBuf>,
    /// 转后台的接手方（Gateway 装配）。空 = 不转。
    detach: Option<Arc<DetachSlot>>,
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
            komo_home: None,
            detach: None,
        }
    }

    /// 子进程里的 `komo` 靠 `KOMO_HOME` 找数据目录，没有它就落到 `~/.komo`，非默认数据目录的
    /// 实例里模型的自查命令会连到另一台 Gateway。这里要的是 Gateway **实际**用的目录，由
    /// 装配处算好递进来，不从环境继承（前台跑的 `KOMO_HOME=… komo gateway --foreground`
    /// 与配置里改过目录的都对）。它是路径，不是凭证。
    pub fn with_komo_home(mut self, home: PathBuf) -> Self {
        self.komo_home = Some(home);
        self
    }

    /// 对话会话里跑久了的命令交给这个槽里的接手方（`docs/monitor.md` §8）。
    pub fn detaching(mut self, slot: Arc<DetachSlot>) -> Self {
        self.detach = Some(slot);
        self
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
                "在自己的进程组里跑一条 bash 命令，返回退出码与输出。默认在会话工作目录里跑，\
                 换目录给 cwd。git、构建、测试、跑程序走它。\n\
                 构建、下载、部署这类慢事照常写成一条命令，timeout_secs 按预计时长给足（省略 = 120）。\
                 要等一个状态（流水线、工单、部署进度）别在这里 sleep 轮询——挂 monitor，脚本在完成或\
                 出错时打一行；对话会话里 10 秒以上的 sleep 会被拒绝。\n\
                 在跟用户对话的会话里，命令跑过 2 分钟还没完会自动转到后台：这次调用先返回「已转后台 #id」，\
                 跑完时退出码和输出尾部会作为一条消息交回这个会话。拿到「已转后台」就先回复用户、结束这一轮\
                 ——别 sleep 等它，也别再发命令去查；要停用 monitor_stop。\n\
                 要中途每出一件事就知道（每个 job 的结果、日志里的每条报错）挂 monitor；要接着做一串事、\
                 最后给结论派 task。\n\
                 你自己就是后台任务时不会转后台，也可以 sleep 轮询，照常在这里等。"
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "要执行的命令，交给 bash -c（没有 bash 的机器上是 /bin/sh -c）" },
                    "cwd": { "type": "string", "description": "工作目录；相对路径按会话工作目录解析" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "description": "时限，秒，按预计时长给足；省略 = 120，上限是 [execution] call_timeout_secs。到点终止进程组（转到后台之后同样从调用开始算）" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            doc: None,
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        let args: BashArgs = parse_args(args, "bash")?;
        if ctx.conversation
            && let Some(sleep) = long_sleep(&args.command)
        {
            return Err(ToolError::Failed {
                message: format!(
                    "对话会话里不在 bash 里 sleep 等（命令里有 `{sleep}`）。等一个状态（流水线、工单、\
                     部署进度）就挂 monitor：脚本在完成或出错时打一行，轮询间隔不短于 15 秒；挂完这一轮先\
                     回复用户。短于 10 秒的 sleep（等服务起来之类）可以留在 bash 里。"
                ),
            });
        }
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
            env: self.environment(ctx),
            timeout,
            output_limit: self.output_limit,
            register: self.register.clone(),
            label: format!("bash · run {} · call {}", ctx.run, ctx.call),
        };

        // 转后台要有接手方，而且得比时限早到点——否则命令先超时，没有"转"的余地。
        let detach_after = ctx.detach_after.filter(|after| {
            *after < timeout
                && self
                    .detach
                    .as_ref()
                    .is_some_and(|slot| slot.get().is_some())
        });
        let started = std::time::Instant::now();
        let driven = run_child_detachable(
            spec,
            sink,
            &ctx.cancel,
            detach_after,
            Some((&ctx.interjected, INTERJECT_GRACE)),
        )
        .await
        .map_err(|error| match error {
            ProcessError::Spawn(message) => ToolError::Failed {
                message: format!("起不来：{message}"),
            },
            ProcessError::Io(message) | ProcessError::Sink(message) => {
                ToolError::Failed { message }
            }
        })?;
        let outcome = match driven {
            Driven::Done(outcome) => outcome,
            Driven::Detached(live) => {
                let (stdout_bytes, stderr_bytes, stdout_excerpt, stderr_excerpt) = live.so_far();
                let detacher = self
                    .detach
                    .as_ref()
                    .and_then(|slot| slot.get())
                    .expect("detach_after 只在有接手方时给");
                let handle = detacher
                    .adopt(DetachedCall {
                        attempt: sink.attempt().clone(),
                        command: args.command.clone(),
                        cwd: cwd.clone(),
                        live: *live,
                    })
                    .await
                    .map_err(|message| ToolError::Failed {
                        message: format!("转后台没成功，命令已终止：{message}"),
                    })?;
                let elapsed = started.elapsed();
                // 没到阈值就交出去的，只能是插话。
                let why = if detach_after.is_some_and(|after| elapsed < after) {
                    "用户插话了"
                } else {
                    "[execution] detach_after_secs"
                };
                let elapsed = elapsed.as_secs();
                let stderr_tail = tail_lines(&stderr_excerpt, 20);
                let stderr_part = if stderr_tail.is_empty() {
                    String::new()
                } else {
                    format!("\n[stderr]\n{stderr_tail}")
                };
                let preview = format!(
                    "已转后台 #{short}：命令跑了 {elapsed} 秒还没完（{why}），\
                     仍在运行；结束时退出码和输出尾部会作为一条消息交回这个会话，要停用 \
                     monitor_stop #{short}。先回复用户、结束这一轮，别 sleep 等它，也别再发命令去查。\n\
                     到现在的输出（stdout={stdout_bytes}B stderr={stderr_bytes}B，只列末尾）：\n{}{stderr_part}",
                    tail_lines(&stdout_excerpt, 40),
                    short = handle.short_id,
                );
                let result = BashResult {
                    command: args.command,
                    cwd: cwd.display().to_string(),
                    exit_code: None,
                    timed_out: false,
                    output_truncated: false,
                    stdout_bytes,
                    stderr_bytes,
                    stdout_excerpt,
                    stderr_excerpt,
                    detached: Some(handle.id),
                };
                return Ok(ToolOutput {
                    status: ToolResultStatus::Completed,
                    result: serde_json::to_value(&result).map_err(|error| ToolError::Failed {
                        message: error.to_string(),
                    })?,
                    exit_code: None,
                    artifacts: vec![],
                    images: vec![],
                    preview: Some(preview),
                });
            }
        };

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
            detached: None,
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
            images: vec![],
            preview: Some(preview),
        })
    }

    // `replay` 用默认的 unsafe：任意命令重启后不重跑（§8.6）。
}

impl BashTool {
    fn environment(&self, ctx: &ToolContext) -> BTreeMap<String, String> {
        let mut env = base_environment();
        if let Some(home) = &self.komo_home {
            env.insert("KOMO_HOME".into(), home.to_string_lossy().into_owned());
        }
        env.insert("KOMO_SESSION".into(), ctx.session.to_string());
        // `!` 没有输入 Run，沿用产物身份以保持 Gateway 子进程识别及拒绝无证据记忆的行为。
        // Agent 工具上下文必须提供 input_run；harness 缺少输入时在执行前报错。
        env.insert(
            "KOMO_RUN".into(),
            ctx.input_run.as_ref().unwrap_or(&ctx.run).to_string(),
        );
        env
    }
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
    use crate::tools::detach::{Detached, Detacher};
    use crate::tools::process::process_group_alive;
    use crate::tools::process::{ChildOutcome, drive};
    use crate::tools::test_support::{context, context_with_cancel, writer};
    use komo_kernel::types::tool::CancelToken;

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

    /// 附录 C.3.3：子进程里的 `komo` 连的是装配时给的数据目录，不是继承来的环境，也不是 `~/.komo`。
    #[tokio::test]
    async fn the_child_gets_the_gateways_data_dir_as_komo_home() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let data_dir = dir.path().join("instance-b");
        let tool = BashTool::new().with_komo_home(data_dir.clone());
        let mut sink = writer(&ctx);
        let plan = tool
            .prepare(
                serde_json::json!({ "command": "printf %s \"$KOMO_HOME\"" }),
                &ctx,
            )
            .await
            .unwrap();
        let output = tool.execute(plan, &ctx, &mut sink).await.unwrap();
        assert_eq!(
            shell_result(&output).stdout_excerpt,
            data_dir.to_string_lossy()
        );

        // 没装配时不带：不从 Gateway 自己的环境里继承一个可能不对的值。
        let bare = BashTool::new();
        let mut sink = writer(&ctx);
        let plan = bare
            .prepare(
                serde_json::json!({ "command": "printf %s \"${KOMO_HOME-unset}\"" }),
                &ctx,
            )
            .await
            .unwrap();
        let output = bare.execute(plan, &ctx, &mut sink).await.unwrap();
        assert_eq!(shell_result(&output).stdout_excerpt, "unset");
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

    /// 对话会话里 10 秒以上的字面量 sleep 在 prepare 就被拒，提示用 monitor；短 sleep、
    /// 非字面量放行；任务会话与 Cron 的 Run（`conversation = false`）照常能 sleep。
    #[tokio::test]
    async fn a_long_sleep_is_refused_in_a_conversation_only() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new();
        let mut conversation = context(dir.path());
        conversation.conversation = true;
        let elsewhere = context(dir.path());

        for command in [
            "for i in $(seq 1 30); do glab ci status; sleep 20; done",
            "sleep 1m",
        ] {
            let error = tool
                .prepare(serde_json::json!({ "command": command }), &conversation)
                .await
                .unwrap_err();
            assert!(
                matches!(&error, ToolError::Failed { message } if message.contains("monitor")),
                "{command}: {error:?}"
            );
            tool.prepare(serde_json::json!({ "command": command }), &elsewhere)
                .await
                .expect("任务会话 / Cron 里放行");
        }
        for command in ["sleep 2 && curl localhost:8080", "sleep $N"] {
            tool.prepare(serde_json::json!({ "command": command }), &conversation)
                .await
                .unwrap_or_else(|error| panic!("{command}: {error:?}"));
        }
    }

    /// 接手方：把活子进程交给自己的任务跑到底，结果从 channel 出来。
    struct Adopter {
        done: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<ChildOutcome>>>,
        stopped: CancelToken,
    }

    #[async_trait]
    impl Detacher for Adopter {
        async fn adopt(&self, call: DetachedCall) -> Result<Detached, String> {
            let tx = self.done.lock().unwrap().take().expect("只接一次");
            let cancel = self.stopped.clone();
            let attempt = call.attempt.clone();
            tokio::spawn(async move {
                let mut sink = komo_kernel::test_support::MemOutputWriter::new(attempt);
                if let Ok(Driven::Done(outcome)) =
                    drive(call.live, &mut sink, &cancel, None, None).await
                {
                    let _ = tx.send(outcome);
                }
            });
            Ok(Detached {
                id: "0199-abcd".into(),
                short_id: "abcd".into(),
            })
        }
    }

    fn adopting() -> (
        BashTool,
        tokio::sync::oneshot::Receiver<ChildOutcome>,
        CancelToken,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let stopped = CancelToken::new();
        let slot = Arc::new(DetachSlot::default());
        slot.set(Arc::new(Adopter {
            done: std::sync::Mutex::new(Some(tx)),
            stopped: stopped.clone(),
        }));
        (BashTool::new().detaching(slot), rx, stopped)
    }

    /// 跑过阈值：这次调用立刻带着「已转后台」收尾（不等命令），进程接着在接手方手里跑完，
    /// 退出码与转后台前后的输出都拿得到。
    #[tokio::test]
    async fn a_command_past_the_threshold_returns_detached_while_it_keeps_running() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = context(dir.path());
        ctx.detach_after = Some(Duration::from_secs(1));
        let (tool, done, _) = adopting();
        let mut sink = writer(&ctx);
        let plan = tool
            .prepare(
                serde_json::json!({ "command": "echo before; sleep 3; echo after; exit 5" }),
                &ctx,
            )
            .await
            .unwrap();

        let started = std::time::Instant::now();
        let output = tool.execute(plan, &ctx, &mut sink).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(2500),
            "没等命令跑完"
        );
        assert_eq!(output.status, ToolResultStatus::Completed);
        assert_eq!(output.exit_code, None);
        assert_eq!(output.result["detached"], "0199-abcd");
        let preview = output.preview.unwrap();
        assert!(preview.starts_with("已转后台 #abcd"), "{preview}");
        assert!(preview.contains("monitor_stop #abcd"), "{preview}");
        assert!(preview.contains("before"), "{preview}");

        let outcome = done.await.expect("接手方跑完了它");
        assert_eq!(outcome.exit_code, Some(5));
        assert!(outcome.stdout_excerpt.contains("before"));
        assert!(outcome.stdout_excerpt.contains("after"));
    }

    /// 用户插话：跑满宽限就转后台，不等阈值；回执说清是插话，信号被这一次用掉。
    #[tokio::test]
    async fn an_interjection_detaches_a_long_command_before_the_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = context(dir.path());
        ctx.detach_after = Some(Duration::from_secs(60));
        ctx.interjected.raise();
        let (tool, done, _) = adopting();
        let mut sink = writer(&ctx);
        let plan = tool
            .prepare(
                serde_json::json!({ "command": "echo before; sleep 8; echo after" }),
                &ctx,
            )
            .await
            .unwrap();

        let started = std::time::Instant::now();
        let output = tool.execute(plan, &ctx, &mut sink).await.unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed >= INTERJECT_GRACE, "宽限内不转：{elapsed:?}");
        assert!(elapsed < Duration::from_secs(6), "没等阈值：{elapsed:?}");
        assert_eq!(output.result["detached"], "0199-abcd");
        let preview = output.preview.unwrap();
        assert!(preview.contains("用户插话了"), "{preview}");
        assert!(!ctx.interjected.is_raised(), "一次插话只换一次转后台");

        let outcome = done.await.expect("接手方跑完了它");
        assert!(outcome.stdout_excerpt.contains("after"));
    }

    /// 宽限内跑完的命令照常收尾；没给阈值（不转后台的会话）插话也不转。信号都还留着。
    #[tokio::test]
    async fn an_interjection_leaves_short_commands_and_undetachable_calls_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = context(dir.path());
        ctx.interjected.raise();
        for detach_after in [Some(Duration::from_secs(60)), None] {
            ctx.detach_after = detach_after;
            let (tool, _done, _) = adopting();
            let mut sink = writer(&ctx);
            let command = if detach_after.is_some() {
                "sleep 1; echo ok"
            } else {
                "sleep 4; echo ok"
            };
            let plan = tool
                .prepare(serde_json::json!({ "command": command }), &ctx)
                .await
                .unwrap();
            let output = tool.execute(plan, &ctx, &mut sink).await.unwrap();
            assert_eq!(output.exit_code, Some(0), "{detach_after:?}");
            assert!(output.result.get("detached").is_none(), "{detach_after:?}");
            assert!(ctx.interjected.is_raised());
        }
    }

    /// 超时从调用开始算：转后台之后到点，接手方那边看到的是超时、进程组被终止。
    #[tokio::test]
    async fn the_timeout_counts_from_the_start_of_the_call_not_from_the_detach() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = context(dir.path());
        ctx.detach_after = Some(Duration::from_secs(1));
        let (tool, done, _) = adopting();
        let mut sink = writer(&ctx);
        let plan = tool
            .prepare(
                serde_json::json!({ "command": "sleep 30", "timeout_secs": 2 }),
                &ctx,
            )
            .await
            .unwrap();
        let output = tool.execute(plan, &ctx, &mut sink).await.unwrap();
        assert!(output.result["detached"].is_string());
        let started = std::time::Instant::now();
        let outcome = done.await.unwrap();
        assert!(outcome.timed_out, "{outcome:?}");
        assert!(
            started.elapsed() < Duration::from_millis(1800),
            "{:?}",
            started.elapsed()
        );
    }

    /// 没有接手方、没给阈值、阈值不早于时限：都不转，照旧在这里等。
    #[tokio::test]
    async fn without_a_detacher_or_a_threshold_it_waits_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = context(dir.path());
        let mut sink = writer(&ctx);
        // 阈值给了但没有接手方。
        ctx.detach_after = Some(Duration::from_millis(200));
        let bare = BashTool::new();
        let plan = bare
            .prepare(serde_json::json!({ "command": "sleep 1; echo ok" }), &ctx)
            .await
            .unwrap();
        let output = bare.execute(plan, &ctx, &mut sink).await.unwrap();
        assert_eq!(output.exit_code, Some(0));

        // 有接手方但这次调用没有阈值（任务会话、Cron、0）。
        ctx.detach_after = None;
        let (tool, _done, _) = adopting();
        let mut sink = writer(&ctx);
        let plan = tool
            .prepare(serde_json::json!({ "command": "sleep 1; echo ok" }), &ctx)
            .await
            .unwrap();
        let output = tool.execute(plan, &ctx, &mut sink).await.unwrap();
        assert_eq!(output.exit_code, Some(0));
        assert!(output.result.get("detached").is_none());
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
