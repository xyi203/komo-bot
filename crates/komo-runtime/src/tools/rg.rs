//! `rg`：在文件里搜正则，返回「路径:行号:正文」（§4）。
//!
//! 搜索用的是 ripgrep 那套**书**——`grep` + `ignore`，就是 [BurntSushi/ripgrep] 仓库拆出来
//! 的 crate——**进程内跑**：不起子进程，也不要求这台机器上装了 `rg`。要书库而不是命令，
//! 是因为外部二进制要么没有、要么各家版本不同；而遍历这件事（隐藏文件、`.gitignore`、
//! 覆盖 glob）本来就该由 `ignore` 这套经过验证的实现负责，不该自己再写一遍。
//!
//! [BurntSushi/ripgrep]: https://github.com/BurntSushi/ripgrep
//!
//! 之所以把它做成工具而不是让模型拼一条 `shell` 命令，为的是**判定**：一次只读搜索的操作
//! 是 [`Operation::ReadFile`]，目标就是被搜的那个路径，于是 §7.1 那两条规则（范围内读
//! Allow、范围外 Ask）原样适用——和 `read` 完全一致。走 `shell` 做不到这一点：任意命令一
//! 律 Ask，一次只读搜索每次都要停下来等人。
//!
//! 遍历与搜索是**阻塞**的，所以跑在 `spawn_blocking` 上，匹配经队列流式写进 `OutputWriter`
//! （§8.3：不把完整输出在进程里堆着）。取消与超时靠一个停止标志让那边收手——`execute` 的
//! future 被丢掉（执行器超时）时标志也会置上，否则那个线程会把整棵树走完。粒度是**每个
//! 文件**（外加每个写出去的匹配）：搜索器自己没有中断钩子，所以一个特别大的单文件要等它
//! 读到能写下东西、或者读完。
//!
//! 恢复方式是 [`RecoveryMode::SafeReread`]：搜索没有副作用，重做只是重新观察（§8.6）。
//!
//! `path` 除了本地目录/文件，还认得 §六 的**根入口**：`skill://`（全部 skill 根）与
//! `artifact://files`（这个会话的产物目录）——**每个真实根一条计划目标**，所以范围判定与
//! 审批看到的还是真的路径。`tool://` 那种现算的正文不是文件，搜索它没有意义（`prepare`
//! 就拒）。

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use grep::printer::StandardBuilder;
use grep::regex::{RegexMatcher, RegexMatcherBuilder};
use grep::searcher::{BinaryDetection, SearcherBuilder};
use ignore::WalkBuilder;
use ignore::overrides::{Override, OverrideBuilder};
use tokio::sync::mpsc;

use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{
    ApprovedPlan, ExecutionPlan, Operation, PlanTarget, PlanVersions, RecoveryMode, TargetAccess,
};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{CancelToken, ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::resources::{self, ResolvedResource, ResourceError};
use super::{normalized, parse_args, plan_time};

/// 一次搜索默认跑多久。比 `shell` 的默认短：搜索在模型这一轮里是等着的动作。
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// 写进输出的字节上限。与 `shell` 同一个口径：**这是防机器资源耗尽，不是防 context 太大**
/// （§4）。碰到它就让搜索停下——把一棵树走完再丢掉结果没有意义。
pub const DEFAULT_OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;
/// 预览里保留多少字节的匹配正文（开头那一段）。
const PREVIEW_HEAD_BYTES: usize = 4096;
/// 一行最多印多少列（`rg --max-columns`，带预览）：JSONL 事件一行动辄几十 KiB，
/// 不截的话一行就能吃掉整份预算。
const MAX_COLUMNS: u64 = 500;
/// 超时后等搜索线程收手多久：它在下一次写入或下一个文件时就会停，这里只是不无限等。
const STOP_GRACE: Duration = Duration::from_secs(1);
/// 队列里一块多大、能排几块。块太小是每个匹配跳一次线程，太大则取消要等这一块写完。
const CHUNK_BYTES: usize = 16 * 1024;
const CHANNEL_DEPTH: usize = 4;
/// 结果里最多记几条读不了的东西（全量还在 stderr 那一份里）。
const MAX_RECORDED_ERRORS: usize = 32;
/// 预览里最多印几条读不了的。
const PREVIEW_ERROR_LINES: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RgArgs {
    pub pattern: String,
    /// 要搜的目录或文件；缺省是会话工作目录。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 只搜匹配这个 glob 的文件，例如 `*.rs`、`!target/**`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    #[serde(default)]
    pub ignore_case: bool,
    /// 把 `pattern` 当字面量——搜 `foo.bar()` 这类正文时必须打开。
    #[serde(default)]
    pub fixed_strings: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RgResult {
    pub pattern: String,
    /// 搜了哪儿：本地路径是解析后的真实路径，资源入口是**逻辑入口**（`skill://`）。
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    /// 印出来过匹配。**false 不是失败**——那就是"搜过了，没有"。
    pub matched: bool,
    #[serde(default)]
    pub output_truncated: bool,
    /// 时限到了、树没走完：已写下的匹配照样交回，但这不是一次完整的搜索。
    #[serde(default)]
    pub timed_out: bool,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    /// 读不了的目录或文件，最多 [`MAX_RECORDED_ERRORS`] 条。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    /// 一共碰到几处（含没记进 `errors` 的那些）。
    #[serde(default)]
    pub error_count: u64,
}

pub struct RgTool {
    default_timeout: Duration,
    output_limit: u64,
}

impl std::fmt::Debug for RgTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RgTool")
            .field("default_timeout", &self.default_timeout)
            .field("output_limit", &self.output_limit)
            .finish_non_exhaustive()
    }
}

impl Default for RgTool {
    fn default() -> Self {
        Self::new()
    }
}

impl RgTool {
    pub fn new() -> Self {
        Self {
            default_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            output_limit: DEFAULT_OUTPUT_LIMIT,
        }
    }

    pub fn with_limits(mut self, timeout: Duration, output_limit: u64) -> Self {
        self.default_timeout = timeout;
        self.output_limit = output_limit;
        self
    }
}

#[async_trait]
impl Tool for RgTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "rg".into(),
            description: "在文件里搜正则（ripgrep），返回「路径:行号:正文」。搜代码和文字用它，\
                 不要用 shell 里的 grep / find / ls 拼搜索。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "ripgrep 语法的正则（Rust regex）；搜字面量时把 fixed_strings 打开" },
                    "path": { "type": "string", "description": "要搜的目录或文件，缺省是工作目录；也可以是资源入口：`skill://`（搜全部 skill 根）、`artifact://files`（本次会话的产物目录）" },
                    "glob": { "type": "string", "description": "只搜匹配这个 glob 的文件，例如 '*.rs' 或 '!target/**'" },
                    "ignore_case": { "type": "boolean", "description": "忽略大小写；缺省区分大小写" },
                    "fixed_strings": { "type": "boolean", "description": "把 pattern 当字面量，不解释正则元字符" }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        let args: RgArgs = parse_args(args, "rg")?;
        if args.pattern.is_empty() {
            return Err(ToolError::InvalidArguments {
                message: "pattern 不能是空串".into(),
            });
        }
        // 正则在这里编一次：**它是参数，不是运行结果**。语法错的模式先撞在这里，模型当场
        // 改一个字重发，而不是先建计划、过审批、再拿回一条失败。（glob 不在这里验：gitignore
        // 那套 glob 是宽松的，写歪了多半只是"什么都没匹配上"，那不是错误。）
        matcher(&args).map_err(|message| ToolError::InvalidArguments { message })?;
        let targets = match &args.path {
            Some(raw) => targets_of(raw, ctx)?,
            // 缺省是这次的工作目录（它一定在）。
            None => vec![PlanTarget::local(ctx.cwd.clone(), TargetAccess::Read)],
        };
        Ok(ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: "rg".into(),
            // 搜索是一次**读**：搜索范围就是计划的目标路径，规则表按它判范围。
            operation: Operation::ReadFile,
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&args)?,
            cwd: Some(ctx.cwd.clone()),
            targets,
            versions: PlanVersions::default(),
            resources: vec![],
            recovery: RecoveryMode::SafeReread,
        })
    }

    async fn execute(
        &self,
        plan: ApprovedPlan,
        ctx: &ToolContext,
        sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let plan = plan.plan();
        let args: RgArgs = parse_args(plan.args.clone(), "rg")?;
        let cwd: PathBuf = plan.cwd.clone().unwrap_or_else(|| ctx.cwd.clone());
        let targets: Vec<PathBuf> = plan
            .targets
            .iter()
            .filter_map(|target| target.path().map(Path::to_path_buf))
            .collect();
        // 计划里没有目标（手工造的、或者旧日志里的）时退回工作目录：**就地搜**总比什么都不
        // 做好，而这不是任何一条规则判断得来的结论。
        let targets = match targets.is_empty() {
            true => vec![cwd.clone()],
            false => targets,
        };
        let scope = match plan.targets.first().and_then(|target| target.uri()) {
            // 资源入口印逻辑入口：模型给的就是它（真实根在计划里，审批看得到）。
            Some(uri) => uri.to_string(),
            None => targets[0].display().to_string(),
        };
        let matcher = matcher(&args).map_err(|message| ToolError::Failed { message })?;
        // 已经取消的调用不该再走树：搜索本身很快就会发现停止标志，但那也要等一个文件。
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        // 这一段走完（正常收尾、超时、或者 future 被丢掉）就置上：阻塞线程据此收手。
        let stop = Arc::new(AtomicBool::new(false));
        let _guard = StopOnDrop(Arc::clone(&stop));

        let job = SearchJob {
            matcher,
            targets: targets.clone(),
            cwd: cwd.clone(),
            glob: args.glob.clone(),
            stop: Arc::clone(&stop),
            cancel: ctx.cancel.clone(),
        };
        let (tx, mut rx) = mpsc::channel::<Chunk>(CHANNEL_DEPTH);
        let walking = tokio::task::spawn_blocking(move || search(&job, &tx));

        let mut stdout_bytes = 0u64;
        let mut stderr_bytes = 0u64;
        let mut truncated = false;
        let mut head: Vec<u8> = Vec::with_capacity(PREVIEW_HEAD_BYTES);
        let drained = tokio::time::timeout(self.default_timeout, async {
            while let Some(chunk) = rx.recv().await {
                if ctx.cancel.is_cancelled() {
                    return Err(ToolError::Cancelled);
                }
                let written = if chunk.stderr {
                    stderr_bytes
                } else {
                    stdout_bytes
                };
                if written >= self.output_limit {
                    // 满了：停止转发，**并让搜索停下**——把一棵树走完再丢掉结果没有意义。
                    truncated = true;
                    stop.store(true, Ordering::Relaxed);
                    continue;
                }
                let room = (self.output_limit - written) as usize;
                let (slice, cut) = match chunk.bytes.len() > room {
                    true => (&chunk.bytes[..room], true),
                    false => (&chunk.bytes[..], false),
                };
                let written = if chunk.stderr {
                    sink.write_stderr(slice).await
                } else {
                    sink.write_stdout(slice).await
                };
                written.map_err(|error| ToolError::Failed {
                    message: error.to_string(),
                })?;
                if chunk.stderr {
                    stderr_bytes += slice.len() as u64;
                } else {
                    stdout_bytes += slice.len() as u64;
                    let room = PREVIEW_HEAD_BYTES.saturating_sub(head.len());
                    head.extend_from_slice(&slice[..slice.len().min(room)]);
                }
                if cut {
                    truncated = true;
                    stop.store(true, Ordering::Relaxed);
                }
            }
            Ok(())
        })
        .await;

        let timed_out = match drained {
            Err(_elapsed) => {
                stop.store(true, Ordering::Relaxed);
                true
            }
            Ok(Err(error)) => return Err(error),
            Ok(Ok(())) => false,
        };
        // 关掉队列：线程若正卡在 `blocking_send` 上，这一下让它报错收手。
        drop(rx);
        let walked = match timed_out {
            false => walking.await.map_err(|error| ToolError::Failed {
                message: format!("搜索线程没能收尾：{error}"),
            })?,
            true => match tokio::time::timeout(STOP_GRACE, walking).await {
                Ok(joined) => joined.map_err(|error| ToolError::Failed {
                    message: format!("搜索线程没能收尾：{error}"),
                })?,
                Err(_elapsed) => SearchOutcome {
                    matched: stdout_bytes > 0,
                    ..SearchOutcome::default()
                },
            },
        };
        // 搜索期间被取消：那一段的输出不作数（执行器的 race 通常已经先答了，这里是兜底）。
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let result = RgResult {
            pattern: args.pattern,
            path: scope,
            glob: args.glob,
            matched: walked.matched,
            output_truncated: truncated,
            timed_out,
            stdout_bytes,
            stderr_bytes,
            errors: walked.errors.clone(),
            error_count: walked.error_count,
        };
        // 有读不了的东西或没走完 = 这次搜索不完整，和 `rg` 的退出码 2 是同一件事。
        let status = if result.error_count == 0 && !result.timed_out {
            ToolResultStatus::Completed
        } else {
            ToolResultStatus::Failed
        };
        let mut preview = format!(
            "rg {}{} · {} · {}{}{} · 已写下 {}B{}\n",
            result.pattern,
            match &result.glob {
                Some(glob) => format!(" --glob {glob}"),
                None => String::new(),
            },
            result.path,
            match result.timed_out {
                true => format!("超时 {}s 未搜完 · ", self.default_timeout.as_secs_f64()),
                false => String::new(),
            },
            match result.error_count {
                0 => String::new(),
                count => format!("{count} 处读不了 · "),
            },
            if result.matched {
                "有匹配"
            } else {
                "无匹配"
            },
            result.stdout_bytes,
            if result.output_truncated {
                "（输出已截断）"
            } else {
                ""
            },
        );
        // 读不了的先排前面：预览留的是**头部**，而失败的原因比命中更该被看见。
        for error in result.errors.iter().take(PREVIEW_ERROR_LINES) {
            preview.push_str(error);
            preview.push('\n');
        }
        preview.push_str(&String::from_utf8_lossy(&head));
        Ok(ToolOutput {
            status,
            result: serde_json::to_value(&result).map_err(|error| ToolError::Failed {
                message: error.to_string(),
            })?,
            exit_code: None,
            artifacts: vec![],
            preview: Some(preview),
        })
    }

    // `verify` 用默认实现——但这条计划本来就不需要它：`SafeReread` 是"重做即重新观察"。
}

/// 这次搜索触及的目标。
///
/// 本地路径一条（解析符号链接，规则只看这个）；资源入口按 §六：
/// **根是每个真实根一条目标**（`skill://`、`artifact://files`），文件就是那一条，而
/// `tool://` 那种现算的正文不是磁盘上的东西——搜不了，当场拒（§六：不进计划就没有执行）。
fn targets_of(raw: &str, ctx: &ToolContext) -> Result<Vec<PlanTarget>, ToolError> {
    let Some(uri) = resources::entry(raw).map_err(ResourceError::into_tool_error)? else {
        let path = super::paths::resolve(raw, &ctx.cwd)?;
        if !path.exists() {
            return Err(ToolError::Failed {
                message: format!("{} 不存在", path.display()),
            });
        }
        return Ok(vec![PlanTarget::local(path, TargetAccess::Read)]);
    };
    match resources::resolve(&uri, &ctx.mounts).map_err(ResourceError::into_tool_error)? {
        ResolvedResource::File(path) => {
            if !path.exists() {
                return Err(ToolError::Failed {
                    message: format!(
                        "{uri} 在本会话里没有对应的文件（{} 不存在）",
                        path.display()
                    ),
                });
            }
            Ok(vec![PlanTarget::resource(
                uri,
                Some(path),
                TargetAccess::Read,
            )])
        }
        // 每个根一条：规则匹配、审批、审计看到的都是真的路径，而逻辑入口还是那一条。
        ResolvedResource::Roots(roots) => Ok(roots
            .into_iter()
            .map(|root| PlanTarget::resource(uri.clone(), Some(root), TargetAccess::Read))
            .collect()),
        ResolvedResource::Virtual(_) => Err(ResourceError::Virtual {
            uri: uri.to_string(),
        }
        .into_tool_error()),
    }
}

/// 模式 + 两个开关 → 匹配器。**语法是 Rust regex**（ripgrep 的默认引擎，也就是书上这一个）：
/// 只连了 `grep-regex`，没有 PCRE2。
fn matcher(args: &RgArgs) -> Result<RegexMatcher, String> {
    RegexMatcherBuilder::new()
        .case_insensitive(args.ignore_case)
        .fixed_strings(args.fixed_strings)
        .build(&args.pattern)
        .map_err(|error| format!("正则不合法：{error}"))
}

/// 一条待写下的字节。`stderr` 那一半放"读不了"这类话——和子进程工具同一个分工。
struct Chunk {
    stderr: bool,
    bytes: Vec<u8>,
}

impl Chunk {
    fn stdout(bytes: Vec<u8>) -> Self {
        Self {
            stderr: false,
            bytes,
        }
    }

    fn stderr(line: String) -> Self {
        Self {
            stderr: true,
            bytes: line.into_bytes(),
        }
    }
}

/// 阻塞线程那一侧的写入器：攒够一块就送进队列。
///
/// `send` 会**阻塞**，于是背压是真的：读取侧写得慢，搜索就走得慢——不会有一方无限地攒。
/// 读取侧不在了（执行器超时把 future 丢掉），`send` 直接报错，搜索顺着错误收手。
struct ChunkWriter {
    tx: mpsc::Sender<Chunk>,
    stop: Arc<AtomicBool>,
    buf: Vec<u8>,
}

impl ChunkWriter {
    fn new(tx: mpsc::Sender<Chunk>, stop: Arc<AtomicBool>) -> Self {
        Self {
            tx,
            stop,
            buf: Vec::with_capacity(CHUNK_BYTES),
        }
    }

    fn send_buf(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK_BYTES));
        self.tx
            .blocking_send(Chunk::stdout(bytes))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "读取侧已经不在了"))
    }
}

/// 打印器看到的那个写入器。**它是个把手**：`search` 留着同一份 `Rc`，收尾时自己 flush
/// ——打印器从不调用 `flush`（它只在自己被 flush 时转发），最后那半块否则会静静地丢掉。
struct SharedWriter(Rc<RefCell<ChunkWriter>>);

impl io::Write for SharedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

impl io::Write for ChunkWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // 不能是 `Interrupted`：`write_all` 会把它当"再试一次"原地重试，线程就停不下来。
        if self.stop.load(Ordering::Relaxed) {
            return Err(io::Error::other("搜索已停止"));
        }
        self.buf.extend_from_slice(bytes);
        if self.buf.len() >= CHUNK_BYTES {
            self.send_buf()?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send_buf()
    }
}

/// 这一段结束就把搜索叫停。**它是一个守卫而不是一句 `store`**：出口有好几条（正常、超时、
/// 报错、future 被丢掉），而漏掉任何一条的代价都是"执行器已经把这次调用扔了，机器还在为它
/// 遍历整棵树"。
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// 阻塞线程上要的全部东西。
struct SearchJob {
    matcher: RegexMatcher,
    /// 这次要搜的全部真实目标：本地路径一条，`skill://` 那种根是每个根一条。
    targets: Vec<PathBuf>,
    cwd: PathBuf,
    glob: Option<String>,
    stop: Arc<AtomicBool>,
    cancel: CancelToken,
}

impl SearchJob {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.cancel.is_cancelled()
    }
}

/// 一次搜索的结果：有没有命中，以及哪些地方读不了。
#[derive(Debug, Default)]
struct SearchOutcome {
    matched: bool,
    errors: Vec<String>,
    error_count: u64,
}

impl SearchOutcome {
    fn note(&mut self, message: String, tx: &mpsc::Sender<Chunk>) {
        self.error_count += 1;
        if self.errors.len() < MAX_RECORDED_ERRORS {
            self.errors.push(message.clone());
        }
        // 也写进 stderr 那一份：完整的问题清单在输出文件里，结果 JSON 只留前几条。
        let _ = tx.blocking_send(Chunk::stderr(format!("{message}\n")));
    }
}

/// 遍历 + 搜索。**这是唯一的阻塞段**，由 [`tokio::task::spawn_blocking`] 扛着。
///
/// 目标可能不止一个（`skill://` 是每个 skill 根一条）：一个接一个搜，顺序就是计划里的顺序
/// ——同一份计划两次搜索印出同样的顺序，这是可复现的一部分。
fn search(job: &SearchJob, tx: &mpsc::Sender<Chunk>) -> SearchOutcome {
    let mut outcome = SearchOutcome::default();
    // glob 锚在各自的根上（越界的模式因此不会跨根误伤），**语法只验一次**：它合不合法与
    // 锚在哪个根上无关。
    let mut overrides: Vec<Option<Override>> = Vec::with_capacity(job.targets.len());
    for target in &job.targets {
        match build_overrides(target, job.glob.as_deref()) {
            Ok(overrides_for) => overrides.push(overrides_for),
            Err(error) => {
                outcome.note(format!("glob 不合法：{error}"), tx);
                return outcome;
            }
        }
    }
    let mut searcher = SearcherBuilder::new()
        // 行号是**搜索器**给的：打印器只负责把 `path:line:text` 拼出来。
        .line_number(true)
        // 二进制文件到第一个 NUL 字节为止——和 `rg` 的默认一样，不让一段二进制灌进模型。
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();
    let writer = Rc::new(RefCell::new(ChunkWriter::new(
        tx.clone(),
        Arc::clone(&job.stop),
    )));
    let mut printer = StandardBuilder::new()
        // 每个匹配一行、带路径与行号，颜色永远不开：`rg --no-heading --line-number --color=never`。
        .heading(false)
        .path(true)
        .max_columns(Some(MAX_COLUMNS))
        .max_columns_preview(true)
        .build_no_color(SharedWriter(Rc::clone(&writer)));

    let mut search_one = |path: &Path, outcome: &mut SearchOutcome| {
        let display = display_path(path, &job.cwd);
        let result = searcher.search_path(
            &job.matcher,
            path,
            printer.sink_with_path(&job.matcher, &display),
        );
        // 停下的时候（取消 / 超时 / 输出满）写入器会拒绝写入，那不是"这个文件坏了"。
        if let Err(error) = result
            && !job.stopped()
        {
            outcome.note(format!("{}: {error}", display.display()), tx);
        }
    };

    for (target, overrides) in job.targets.iter().zip(overrides) {
        if job.stopped() {
            break;
        }
        // 目标本身就是个文件（含指向文件的链接）时直接搜它——`ignore` 的遍历只认目录树。
        if target.is_file() {
            let excluded = overrides
                .as_ref()
                .is_some_and(|overrides| overrides.matched(target, false).is_ignore());
            if !excluded {
                search_one(target, &mut outcome);
            }
            continue;
        }
        let mut builder = WalkBuilder::new(target);
        builder
            // 隐藏文件与 `.gitignore` 照 ripgrep 的**默认**走：跳过隐藏文件；`.gitignore`
            // 只在 git 仓库里生效（`require_git` 的默认就是 true）。这里不改成"哪儿都认
            // `.gitignore`"——模型对 rg 的预期就是这一套，而 `$HOME/.gitignore` 那种东西
            // 悄悄吃掉搜索结果，比多搜几个文件难查得多。
            .hidden(true)
            .git_ignore(true)
            // 串行 + 按路径排序：同一棵树两次搜索印出同样的顺序。模型读到的是文件的先后，
            // 而文件系统的返回顺序是它自己的事——代价是慢一点，换的是可复现。
            .sort_by_file_path(|a, b| a.cmp(b));
        if let Some(overrides) = overrides {
            builder.overrides(overrides);
        }
        for entry in builder.build() {
            if job.stopped() {
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    outcome.note(error.to_string(), tx);
                    continue;
                }
            };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let path = entry.path().to_path_buf();
            search_one(&path, &mut outcome);
        }
    }
    // 收尾：把最后那半块送出去。打印器自己不会 flush，漏掉这一步就是**静静丢数据**。
    let _ = io::Write::flush(&mut *writer.borrow_mut());
    outcome.matched = printer.has_written();
    outcome
}

/// 打印出来的路径：能相对工作目录说清就用相对的。
///
/// 绝对路径会把模型看到的那点篇幅大半花在前缀上，而搜索范围本来就写在结果抬头里。
fn display_path(path: &Path, cwd: &Path) -> PathBuf {
    path.strip_prefix(cwd)
        .map(|rest| match rest.as_os_str().is_empty() {
            true => PathBuf::from("."),
            false => rest.to_path_buf(),
        })
        .unwrap_or_else(|_| path.to_path_buf())
}

fn build_overrides(root: &Path, glob: Option<&str>) -> Result<Option<Override>, String> {
    let Some(glob) = glob else {
        return Ok(None);
    };
    let mut builder = OverrideBuilder::new(root);
    builder.add(glob).map_err(|error| error.to_string())?;
    builder.build().map(Some).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{approved, context, writer};

    fn rg_result(output: &ToolOutput) -> RgResult {
        serde_json::from_value(output.result.clone()).expect("rg 的结果")
    }

    fn preview(output: &ToolOutput) -> String {
        output.preview.clone().expect("预览")
    }

    /// 一个有那么几个文件和几处匹配的工作目录。
    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "foo bar\nFOO baz\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "nothing here\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("c.rs"), "foo\nfoo2\n").unwrap();
        dir
    }

    async fn run(tool: &RgTool, dir: &Path, args: serde_json::Value) -> (ToolOutput, String) {
        let ctx = context(dir);
        let mut sink = writer(&ctx);
        let plan = tool.prepare(args, &ctx).await.unwrap();
        let output = tool.execute(approved(plan), &ctx, &mut sink).await.unwrap();
        let preview = preview(&output);
        (output, preview)
    }

    #[tokio::test]
    async fn a_search_is_planned_as_a_read_over_the_path_it_searches() {
        let dir = workspace();
        let ctx = context(dir.path());
        let plan = RgTool::new()
            .prepare(serde_json::json!({ "pattern": "foo" }), &ctx)
            .await
            .unwrap();
        assert_eq!(plan.operation, Operation::ReadFile);
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].path(), Some(ctx.cwd.as_path()));
        assert_eq!(plan.targets[0].access, TargetAccess::Read);
        assert_eq!(plan.recovery, RecoveryMode::SafeReread);

        let plan = RgTool::new()
            .prepare(serde_json::json!({ "pattern": "foo", "path": "sub" }), &ctx)
            .await
            .unwrap();
        assert_eq!(
            plan.targets[0].path(),
            Some(
                std::fs::canonicalize(dir.path().join("sub"))
                    .unwrap()
                    .as_path()
            ),
            "搜子目录时目标是子目录，范围判定跟着它走"
        );
    }

    /// `path` 是资源入口时**每个真实根一条目标**：规则匹配、审批、审计看到的都是真的路径，
    /// 而逻辑入口还是那一条（`skill://`）。
    #[tokio::test]
    async fn a_skill_root_search_plans_one_target_per_skill_root() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        let home = dir.path().join("home");
        for (root, body) in [
            (&shared, "review: 检查清单\n"),
            (&home, "deploy: 上线步骤\n"),
        ] {
            std::fs::create_dir_all(root.join("review")).unwrap();
            std::fs::write(root.join("review/SKILL.md"), body).unwrap();
        }
        let shared = std::fs::canonicalize(&shared).unwrap();
        let home = std::fs::canonicalize(&home).unwrap();

        let mut ctx = context(dir.path());
        ctx.mounts.skill_dirs = vec![shared.clone(), home.clone()];
        let tool = RgTool::new();
        let plan = tool
            .prepare(
                serde_json::json!({ "pattern": "上线", "path": "skill://" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(plan.targets.len(), 2, "两条根 → 两条目标");
        assert_eq!(
            plan.targets
                .iter()
                .map(|target| target.describe())
                .collect::<Vec<_>>(),
            vec![
                format!("skill://（{}）", shared.display()),
                format!("skill://（{}）", home.display()),
            ],
            "两条目标共用同一条逻辑入口，真实目标各是各的"
        );

        let mut sink = writer(&ctx);
        let output = tool.execute(approved(plan), &ctx, &mut sink).await.unwrap();
        let printed = preview(&output);
        let result = rg_result(&output);
        assert_eq!(result.path, "skill://", "抬头印逻辑入口");
        assert!(result.matched);
        assert!(
            printed.contains("review/SKILL.md:1:deploy: 上线步骤"),
            "{printed}"
        );
    }

    /// `artifact://files` 的根是本次会话的产物目录；给到具体文件时只搜那一个。
    #[tokio::test]
    async fn an_artifact_root_is_this_sessions_artifacts_directory() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("s1");
        std::fs::create_dir_all(session.join("artifacts")).unwrap();
        std::fs::write(session.join("artifacts/report.md"), "结论：foo\n").unwrap();

        let mut ctx = context(dir.path());
        ctx.mounts.session_root = Some(std::fs::canonicalize(&session).unwrap());
        let tool = RgTool::new();

        let plan = tool
            .prepare(
                serde_json::json!({ "pattern": "foo", "path": "artifact://files" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(
            plan.targets[0].describe(),
            format!(
                "artifact://files（{}）",
                std::fs::canonicalize(session.join("artifacts"))
                    .unwrap()
                    .display()
            )
        );

        let mut sink = writer(&ctx);
        let output = tool.execute(approved(plan), &ctx, &mut sink).await.unwrap();
        assert!(
            preview(&output).contains("report.md:1:结论：foo"),
            "{}",
            preview(&output)
        );

        let plan = tool
            .prepare(
                serde_json::json!({ "pattern": "foo", "path": "artifact://files/report.md" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(plan.targets.len(), 1);
        assert!(plan.targets[0].uri().is_some());
        assert_eq!(
            plan.targets[0].path(),
            Some(
                std::fs::canonicalize(session.join("artifacts/report.md"))
                    .unwrap()
                    .as_path()
            )
        );
        let mut sink = writer(&ctx);
        let output = tool.execute(approved(plan), &ctx, &mut sink).await.unwrap();
        assert_eq!(rg_result(&output).path, "artifact://files/report.md");
    }

    /// 现算的正文不是磁盘上的文件：搜索它没有意义，`prepare` 就拒。
    #[tokio::test]
    async fn a_virtual_entry_is_not_searchable() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = context(dir.path());
        ctx.mounts.tools = std::sync::Arc::from([]);
        let error = RgTool::new()
            .prepare(
                serde_json::json!({ "pattern": "foo", "path": "tool://" }),
                &ctx,
            )
            .await
            .unwrap_err();
        let ToolError::Failed { message } = &error else {
            panic!("{error:?}")
        };
        assert!(message.contains("现算出来的正文"), "{message}");
        assert!(message.contains("read"), "{message}");
    }

    #[tokio::test]
    async fn a_match_comes_back_with_its_file_and_line() {
        let dir = workspace();
        let (output, printed) = run(
            &RgTool::new(),
            dir.path(),
            serde_json::json!({ "pattern": "foo2" }),
        )
        .await;

        assert_eq!(output.status, ToolResultStatus::Completed);
        let result = rg_result(&output);
        assert!(result.matched);
        assert!(
            printed.ends_with("sub/c.rs:2:foo2\n"),
            "路径相对工作目录、带行号：{printed}"
        );
        assert_eq!(
            result.stdout_bytes,
            "sub/c.rs:2:foo2\n".len() as u64,
            "只写下命中的那一行"
        );
    }

    #[tokio::test]
    async fn searching_a_single_file_works_too() {
        let dir = workspace();
        let (output, printed) = run(
            &RgTool::new(),
            dir.path(),
            serde_json::json!({ "pattern": "foo", "path": "a.txt" }),
        )
        .await;
        assert!(rg_result(&output).matched);
        assert!(printed.contains("a.txt:1:foo bar"), "{printed}");
        assert!(!printed.contains("c.rs"), "只搜点名的那个文件：{printed}");
    }

    #[tokio::test]
    async fn no_match_is_a_result_not_a_failure() {
        let dir = workspace();
        let (output, printed) = run(
            &RgTool::new(),
            dir.path(),
            serde_json::json!({ "pattern": "找不到的东西" }),
        )
        .await;

        assert_eq!(output.status, ToolResultStatus::Completed);
        let result = rg_result(&output);
        assert!(!result.matched);
        assert_eq!(result.stdout_bytes, 0);
        assert!(printed.contains("无匹配"), "{printed}");
    }

    /// 正则的语法错误是**参数错误**：它撞在 prepare 上，模型改一个字就能重发，不用先建
    /// 计划、过审批、再拿回一条失败。
    #[tokio::test]
    async fn a_bad_pattern_is_rejected_before_any_plan() {
        let dir = workspace();
        let ctx = context(dir.path());
        let error = RgTool::new()
            .prepare(serde_json::json!({ "pattern": "(" }), &ctx)
            .await
            .unwrap_err();
        let ToolError::InvalidArguments { message } = &error else {
            panic!("{error:?}")
        };
        assert!(message.contains("正则"), "{message}");
    }

    /// glob 什么都没匹配上（包括写歪了的 glob）是**结果**，不是失败：gitignore 那套 glob
    /// 是宽松的，`无匹配` 就是它该给的答案。
    #[tokio::test]
    async fn a_glob_that_matches_nothing_is_a_result_not_a_failure() {
        let dir = workspace();
        let (output, printed) = run(
            &RgTool::new(),
            dir.path(),
            serde_json::json!({ "pattern": "foo", "glob": "*.nope" }),
        )
        .await;
        assert_eq!(output.status, ToolResultStatus::Completed);
        let result = rg_result(&output);
        assert!(!result.matched);
        assert_eq!(result.error_count, 0);
        assert!(printed.contains("无匹配"), "{printed}");
    }

    #[tokio::test]
    async fn a_path_that_is_not_there_is_a_named_failure() {
        let dir = workspace();
        let ctx = context(dir.path());
        let error = RgTool::new()
            .prepare(
                serde_json::json!({ "pattern": "foo", "path": "nope" }),
                &ctx,
            )
            .await
            .unwrap_err();
        let ToolError::Failed { message } = &error else {
            panic!("{error:?}")
        };
        assert!(message.contains("不存在"), "{message}");
    }

    #[tokio::test]
    async fn the_glob_and_flags_limit_what_is_searched() {
        let dir = workspace();
        let (_, printed) = run(
            &RgTool::new(),
            dir.path(),
            serde_json::json!({ "pattern": "foo", "glob": "*.rs" }),
        )
        .await;
        assert!(printed.contains("sub/c.rs:1:foo"), "{printed}");
        assert!(
            !printed.contains("a.txt"),
            "glob 之外的文件不该被搜：{printed}"
        );

        // 默认区分大小写：`FOO` 只命中那一行。
        let (output, printed) = run(
            &RgTool::new(),
            dir.path(),
            serde_json::json!({ "pattern": "FOO" }),
        )
        .await;
        assert!(printed.contains("a.txt:2:FOO baz"), "{printed}");
        assert!(!printed.contains("foo bar"), "{printed}");
        assert_eq!(
            rg_result(&output).stdout_bytes,
            "a.txt:2:FOO baz\n".len() as u64
        );

        // 打开 `ignore_case`：小写的那些也算命中。
        let (output, printed) = run(
            &RgTool::new(),
            dir.path(),
            serde_json::json!({ "pattern": "FOO", "ignore_case": true }),
        )
        .await;
        assert_eq!(rg_result(&output).stdout_bytes, {
            let lines = [
                "a.txt:1:foo bar",
                "a.txt:2:FOO baz",
                "sub/c.rs:1:foo",
                "sub/c.rs:2:foo2",
            ];
            lines.iter().map(|line| line.len() as u64 + 1).sum::<u64>()
        });
        assert!(printed.contains("sub/c.rs:2:foo2"), "{printed}");

        // 字面量模式让 `foo b` 这种带元字符的正文搜得到。
        let (_, printed) = run(
            &RgTool::new(),
            dir.path(),
            serde_json::json!({ "pattern": "foo b", "fixed_strings": true }),
        )
        .await;
        assert!(printed.contains("a.txt:1:foo bar"), "{printed}");
    }

    /// 遍历用的是 `ignore`：隐藏文件跳过，`.gitignore` 在 git 仓库里生效（`rg` 的默认）。
    /// 这条是"为什么用这个库而不是自己走目录"的落点。
    #[tokio::test]
    async fn hidden_and_ignored_files_are_skipped_like_ripgrep_does() {
        let dir = workspace();
        std::fs::write(dir.path().join(".hidden.txt"), "foo\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "foo\n").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        // `.gitignore` 只在仓库里生效：`rg` 不认 git 仓库之外的那些忽略文件。
        std::fs::create_dir(dir.path().join(".git")).unwrap();

        let (_, printed) = run(
            &RgTool::new(),
            dir.path(),
            serde_json::json!({ "pattern": "foo" }),
        )
        .await;
        assert!(!printed.contains(".hidden.txt"), "{printed}");
        assert!(!printed.contains("ignored.txt"), "{printed}");
        assert!(printed.contains("a.txt:1:foo bar"), "{printed}");

        // 不在仓库里时同一条 `.gitignore` 不生效——这才是 `rg` 的行为。
        std::fs::remove_dir(dir.path().join(".git")).unwrap();
        let (_, printed) = run(
            &RgTool::new(),
            dir.path(),
            serde_json::json!({ "pattern": "foo" }),
        )
        .await;
        assert!(printed.contains("ignored.txt:1:foo"), "{printed}");
    }

    #[tokio::test]
    async fn the_output_limit_stops_the_search_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let mut many = String::new();
        for i in 0..500 {
            many.push_str(&format!("foo {i}\n"));
        }
        std::fs::write(dir.path().join("many.txt"), many).unwrap();
        let tool = RgTool::new().with_limits(Duration::from_secs(30), 1024);
        let (output, printed) =
            run(&tool, dir.path(), serde_json::json!({ "pattern": "foo" })).await;

        let result = rg_result(&output);
        assert!(result.matched);
        assert!(result.output_truncated, "碰到上限要说出来：{printed}");
        assert!(
            result.stdout_bytes <= 1024,
            "写下 {} 字节，超过了上限",
            result.stdout_bytes
        );
        assert!(printed.contains("输出已截断"), "{printed}");
    }

    /// 匹配多到要分好几块送：上限在第一块就满了，搜索线程随后的写入被拒。那一下拒绝曾经
    /// 用的是 `Interrupted`，而 `write_all` 碰到它会**原地重试**——线程空转、队列不关，
    /// 这次调用只能等满时限，拿回一句"超时"（真实会话里 8 MiB 写满之后跑到 30s）。
    #[tokio::test]
    async fn hitting_the_output_limit_mid_search_returns_promptly_with_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let line = "foo match\n".repeat(20_000);
        std::fs::write(dir.path().join("many.txt"), &line).unwrap();
        std::fs::write(dir.path().join("more.txt"), &line).unwrap();
        let tool = RgTool::new().with_limits(Duration::from_secs(5), 1024);
        let started = std::time::Instant::now();
        let (output, printed) =
            run(&tool, dir.path(), serde_json::json!({ "pattern": "foo" })).await;

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "上限满了就该停：{:?}",
            started.elapsed()
        );
        assert_eq!(output.status, ToolResultStatus::Completed);
        let result = rg_result(&output);
        assert!(result.output_truncated, "{printed}");
        assert!(!result.timed_out);
        assert_eq!(result.stdout_bytes, 1024);
        assert!(printed.contains("many.txt:1:foo match"), "{printed}");
    }

    /// 一行 JSONL 事件动辄几十 KiB：一行不能吃掉整份预算，按 `rg --max-columns` 截成开头一段。
    #[tokio::test]
    async fn a_very_long_matching_line_is_cut_to_a_preview() {
        let dir = tempfile::tempdir().unwrap();
        let long = format!(
            "{{\"type\":\"cron\",\"data\":\"{}\"}}\n",
            "x".repeat(100_000)
        );
        std::fs::write(
            dir.path().join("events.jsonl"),
            format!("{long}short cron\n"),
        )
        .unwrap();
        let (output, printed) = run(
            &RgTool::new(),
            dir.path(),
            serde_json::json!({ "pattern": "cron" }),
        )
        .await;

        let result = rg_result(&output);
        assert!(
            result.stdout_bytes < (MAX_COLUMNS as u64) * 2,
            "长行没被截：写下 {} 字节",
            result.stdout_bytes
        );
        assert!(
            printed.contains("events.jsonl:1:{\"type\":\"cron\""),
            "截断保留开头：{printed}"
        );
        assert!(printed.contains("omitted"), "说出被截了：{printed}");
        assert!(
            printed.contains("events.jsonl:2:short cron"),
            "短行照常：{printed}"
        );
    }

    /// 写得慢的输出存储：每一块都要等一会儿，搜索因此走不完时限。
    struct SlowWriter(komo_kernel::test_support::MemOutputWriter);

    #[async_trait]
    impl OutputWriter for SlowWriter {
        async fn write_stdout(
            &mut self,
            chunk: &[u8],
        ) -> Result<(), komo_kernel::traits::StoreError> {
            tokio::time::sleep(Duration::from_millis(100)).await;
            self.0.write_stdout(chunk).await
        }

        async fn write_stderr(
            &mut self,
            chunk: &[u8],
        ) -> Result<(), komo_kernel::traits::StoreError> {
            self.0.write_stderr(chunk).await
        }

        fn attempt(&self) -> &komo_kernel::types::refs::AttemptRef {
            self.0.attempt()
        }

        fn bytes_written(&self) -> u64 {
            self.0.bytes_written()
        }
    }

    /// 时限到了，已经找到的不扔：结果带着那部分匹配、标明超时未搜完，而不是一句光秃秃的"超时"。
    #[tokio::test]
    async fn a_timeout_returns_what_was_found_so_far_marked_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("many.txt"), "foo match\n".repeat(50_000)).unwrap();
        let ctx = context(dir.path());
        let mut sink = SlowWriter(writer(&ctx));
        let tool = RgTool::new().with_limits(Duration::from_millis(350), DEFAULT_OUTPUT_LIMIT);
        let plan = tool
            .prepare(serde_json::json!({ "pattern": "foo" }), &ctx)
            .await
            .unwrap();
        let started = std::time::Instant::now();
        let output = tool.execute(approved(plan), &ctx, &mut sink).await.unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(output.status, ToolResultStatus::Failed, "没搜完就不是完成");
        let result = rg_result(&output);
        assert!(result.timed_out);
        assert!(result.matched);
        assert!(result.stdout_bytes > 0);
        assert_eq!(result.stdout_bytes, sink.0.stdout.len() as u64);
        let printed = preview(&output);
        assert!(printed.contains("超时"), "{printed}");
        assert!(printed.contains("many.txt:1:foo match"), "{printed}");
    }

    #[tokio::test]
    async fn a_cancelled_call_does_not_walk_the_tree() {
        let dir = workspace();
        let cancel = CancelToken::new();
        cancel.cancel();
        let ctx = crate::tools::test_support::context_with_cancel(dir.path(), cancel);
        let mut sink = writer(&ctx);
        let tool = RgTool::new();
        let plan = tool
            .prepare(serde_json::json!({ "pattern": "foo" }), &ctx)
            .await
            .unwrap();
        let error = tool
            .execute(approved(plan), &ctx, &mut sink)
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Cancelled), "{error:?}");
    }
}
