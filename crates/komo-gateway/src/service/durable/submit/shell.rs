//! `!` 命令直跑（`docs/storage-durable-d5.md` 决定 9）：gateway 直接用 `bash` 工具跑一条命令，
//! **不建 harness run**、不发模型请求；完整输出落 blob，再提交一条 `shell.command` write
//! （带 `model`：下一轮模型看得见这条命令与它的输出）。不做崩溃重放：进程在命令跑到一半时没了，
//! 命令就是没跑完——没有 submission 记下它，也不会被重新跑。
//!
//! # 转后台
//!
//! 命令跑在 Gateway 自己的后台任务里，HTTP 请求只等 `[execution] detach_after_secs`：到点还没完就先回
//! 一张“已转后台”的回执（[`ShellReceipt::Detached`]），命令接着跑，跑完照常落 blob、提交那条
//! `shell.command` write——**不起模型 Run**，也不走 `bash` 工具那条“登记监听、结束时交回一条输入”的
//! 路（`docs/monitor.md` §8）：`!` 的结果本来就是一条只进历史的 write。还没落账的命令只记在内存里
//! （[`RunningShells`]）：同一请求键重发认得出它；正常停机时终止它的进程组，把到那时为止的输出
//! 连同一句“Gateway 停机时被终止”落进历史。崩溃（进程被杀、断电）时它就丢了：历史里什么都没有。
//!
//! 命令型 cron Job（W3b）用同一个 [`run_command`]：先跑出结果，再按自己的方式落账。
//!
//! # 一条 `!` 命令的 RunId
//!
//! write submission 的十进制 ID：[`DurableCore::run_detail`](super::DurableCore::run_detail) 认它——
//! 落下了就是 `Completed`（`final_message` = 送给模型的那份输出），遇忙排在收件箱里就是 `Queued`。
//! 命令本身**立刻**跑（不等前面的 run）：它排的是"这条消息什么时候进历史"，不是"什么时候执行"。
//!
//! # 输出落在哪
//!
//! `tool-output/{conversation}/shell-{uuid}/1/`：与 harness 的工具输出同一棵目录树
//! （`tool-output/{conversation}/{tool_task}/{attempt}/`），`call` 一级换成 `shell-…`，所以
//! 不会与任何工具任务的目录撞；`output.json` / stdout / stderr 的引用写在 entry 的 `data` 里，
//! 复制会话（`/fork` `/clone`）照样整份带走。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use komo_kernel::durable::{ConversationId, Message};
use komo_kernel::projection::{ToolResultFacts, project_views, select};
use komo_kernel::protocol::config::ExecutionConfig;
use komo_kernel::traits::{OutputWriter, ToolOutputStore};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::{AttemptId, RunId, ToolCallId};
use komo_kernel::types::refs::{AttemptRef, ToolResultBody, ToolResultStatus};
use komo_kernel::types::tool::{CancelToken, InterjectSignal, ToolContext, ToolError, ToolOutput};
use komo_runtime::durable::harness::{SubmissionDraft, WriteDraft};
use komo_store::{FileToolOutputStore, SessionPaths};
use serde_json::{Value, json};

use super::super::DurableCore;
use super::super::hooks::{self, env, tools};
use super::super::sessions::SessionError;
use super::{Accepted, Landing};
use crate::service::state::GatewayState;

/// `!` 命令放进会话的 entry 的 kind（`docs/storage-durable-d5.md` §2：gateway 写的 `shell.command`）。
pub const SHELL_ENTRY: &str = komo_kernel::durable::legacy_events::SHELL_ENTRY_KIND;

/// 兜底计时器比 `call_timeout` 多给的一段：工具自己守时限，终止进程组与写完输出还要时间
/// （同 harness 的 `REAP_GRACE`）。
const REAP_GRACE: Duration = Duration::from_secs(15);

/// 停机时等被终止的 `!` 命令把那条说明落进历史，最多等这么久。
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

/// 一条还没落账的 `!` 命令。
#[derive(Debug, Clone)]
struct RunningShell {
    /// 内容哈希（同一请求键重发时比内容）。
    hash: String,
    /// 输出目录里 `call` 一级的名字，也是转后台回执里的临时身份。
    call: String,
    cancel: CancelToken,
}

/// 还没落账的 `!` 命令，按 `(session, 请求键)`。**只在内存里**：Gateway 一没，它们就没了（不做崩溃重放）。
#[derive(Debug, Default)]
pub struct RunningShells {
    inner: Mutex<Shells>,
}

#[derive(Debug, Default)]
struct Shells {
    running: HashMap<(String, String), RunningShell>,
    /// 开始停机了：不再接新的。
    closing: bool,
}

/// [`RunningShells::claim`] 的结果。
enum Claim {
    /// 占到了：这条由我来跑。
    Mine,
    /// 同一请求键的那条还在跑。
    Running(RunningShell),
}

impl RunningShells {
    fn claim(&self, session: &str, key: &str, shell: &RunningShell) -> Result<Claim, SessionError> {
        let mut shells = self.inner.lock().expect("`!` 命令表");
        if shells.closing {
            return Err(SessionError::Internal("Gateway 正在停机".into()));
        }
        let slot = (session.to_owned(), key.to_owned());
        if let Some(existing) = shells.running.get(&slot) {
            return Ok(Claim::Running(existing.clone()));
        }
        shells.running.insert(slot, shell.clone());
        Ok(Claim::Mine)
    }

    fn release(&self, session: &str, key: &str) {
        self.inner
            .lock()
            .expect("`!` 命令表")
            .running
            .remove(&(session.to_owned(), key.to_owned()));
    }

    fn is_empty(&self) -> bool {
        self.inner.lock().expect("`!` 命令表").running.is_empty()
    }

    /// 停机：不再接新的，终止还在跑的（它们各自把“停机时被终止”落进历史），最多等
    /// [`SHUTDOWN_WAIT`]。要在提交线关闭之前做。
    pub async fn shutdown(&self) {
        {
            let mut shells = self.inner.lock().expect("`!` 命令表");
            shells.closing = true;
            for shell in shells.running.values() {
                shell.cancel.cancel();
            }
        }
        let deadline = tokio::time::Instant::now() + SHUTDOWN_WAIT;
        while !self.is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// 从表里销号：落账之后（或者跑不下去）。放在 Drop 里，任务怎么出去都销。
struct Release {
    core: Arc<DurableCore>,
    session: String,
    key: String,
}

impl Drop for Release {
    fn drop(&mut self) {
        self.core.shells.release(&self.session, &self.key);
    }
}

/// `!` 命令的受理结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellReceipt {
    /// 跑完了，`shell.command` 已提交（遇忙时排在收件箱里）。
    Landed(Accepted),
    /// 跑过 `detach_after_secs` 还没完：命令在后台接着跑，跑完再提交。`call` 是它的临时身份，
    /// `deduplicated` = 这是同一请求键的重发。
    Detached { call: String, deduplicated: bool },
}

/// 一条直跑命令的结果。
#[derive(Debug, Clone)]
pub struct ShellRun {
    pub command: String,
    pub status: ToolResultStatus,
    pub exit_code: Option<i32>,
    pub elapsed_ms: u64,
    /// 模型视图：与同一条命令作为 `bash` 工具结果时逐字相同（同一处投影，超限给可 `read` 的引用）。
    pub view: String,
    /// 落进 entry 的 `data`：`output.json` / stdout / stderr 的引用与耗时。
    pub data: Value,
}

/// 工具结果 → `output.json` 的正文（同 runtime 执行器的 `result_body`，那份不对外）。
fn result_body(outcome: &Result<ToolOutput, ToolError>) -> ToolResultBody {
    match outcome {
        Ok(output) => ToolResultBody {
            status: output.status,
            result: output.result.clone(),
            error: None,
            exit_code: output.exit_code,
            artifacts: output.artifacts.clone(),
            images: vec![],
            preview: output.preview.clone(),
        },
        Err(error) => ToolResultBody {
            status: if error.is_uncertain() {
                ToolResultStatus::Uncertain
            } else {
                ToolResultStatus::Failed
            },
            result: Value::Null,
            error: Some(error.to_string()),
            exit_code: None,
            artifacts: vec![],
            images: vec![],
            // 工具失败时模型必须看见那句话。
            preview: Some(error.to_string()),
        },
    }
}

/// 在 `conversation` 的环境里（工作目录、能力面、受保护路径与 `bash` 同一份）直接跑一条命令，
/// 完整输出落 blob。命令自己失败（退出码非 0、超时、起不来）不是错误——那是 [`ShellRun`] 里的
/// `status`；这里的 `Err` 只有"这个 Agent 没有 bash 工具"、输出落不了盘这类跑不起来的情形。
///
/// `call` 是输出目录里 `call` 一级的名字（不同命令不同）；`cancel` 举起时 `bash` 终止进程组。
/// `limit` 是这条命令的时限（[`Limit`]）。
pub async fn run_command(
    state: &Arc<GatewayState>,
    conversation: ConversationId,
    command: &str,
    call: &str,
    cancel: &CancelToken,
    limit: Limit,
) -> Result<ShellRun, SessionError> {
    let core = state
        .core()
        .map_err(|error| SessionError::Internal(error.to_string()))?;
    let config = state.snapshot();
    let resolved = hooks::resolve(state, &*core.storage, conversation, &config)
        .await
        .map_err(|error| SessionError::Internal(error.to_string()))?;

    let catalog = state.tool_executor.tool_names();
    let surface = tools::surface_for(state, &resolved, &catalog);
    if !surface.allows("bash") {
        return Err(SessionError::Invalid(format!(
            "这个会话的 Agent（{}）没有 bash 工具，`!` 用不了",
            resolved.agent_id
        )));
    }
    let bash = state
        .tool_executor
        .tools()
        .get("bash")
        .cloned()
        .ok_or_else(|| SessionError::Internal("执行器里没有 bash 工具".into()))?;

    let session_id = resolved.session.clone();
    let store = Arc::new(FileToolOutputStore::new(
        SessionPaths::new(&config.paths.sessions_dir, &session_id),
        session_id.clone(),
    ));
    let tool_env = env::build(
        state,
        &config,
        &resolved,
        &surface,
        Arc::clone(&store) as Arc<dyn ToolOutputStore>,
    );
    let attempt = AttemptRef {
        session: session_id.clone(),
        run: RunId::from_raw(conversation.to_string()),
        call: ToolCallId::from_raw(call),
        attempt: AttemptId::from_raw("1"),
    };
    let ctx = ToolContext {
        session: session_id,
        run: attempt.run.clone(),
        input_run: None,
        call: attempt.call.clone(),
        attempt: attempt.attempt.clone(),
        source: tool_env.source.clone(),
        cwd: tool_env.cwd.clone(),
        roots: tool_env.roots.clone(),
        mounts: tool_env.mounts.clone(),
        resumed: None,
        cancel: cancel.clone(),
        call_timeout: tool_env.call_timeout,
        // `bash` 自己不转后台：`!` 的转后台在 [`submit_shell`] 那一层（整条命令本来就跑在后台任务里）。
        // 也没有"禁长 sleep"这条对话会话的规矩。
        detach_after: None,
        interjected: InterjectSignal::new(),
        conversation: false,
    };

    let mut writer = store
        .begin(&attempt)
        .await
        .map_err(|error| SessionError::Internal(format!("输出目录建不出来：{error}")))?;
    let started = std::time::Instant::now();
    let args = bash_args(command, limit, &config.execution);
    let outcome = run_tool(&*bash, args, &ctx, writer.as_mut()).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    // 完整输出先落盘（同步 → 改名 → 同步目录），之后才提交引用它的 entry。
    let mut published = store
        .publish(writer, result_body(&outcome))
        .await
        .map_err(|error| SessionError::Internal(format!("输出落不了盘：{error}")))?;
    published.elapsed_ms = elapsed_ms;

    let body = published
        .body
        .as_deref()
        .ok_or_else(|| SessionError::Internal("发布的输出没带正文".into()))?;
    let facts = ToolResultFacts {
        tool: "bash",
        status: published.status,
        elapsed_ms,
        text: body.preview.as_deref(),
        output: &published.output,
        stdout: published.stdout.as_ref(),
        stderr: published.stderr.as_ref(),
        artifacts: &body.artifacts,
        images: &body.images,
    };
    // 刚跑完的结果从不处于已衰减状态（同 harness 的 `render`）。
    let view = select(project_views(&facts, &config.execution.projection()), false);
    let mut data = json!({
        "command": command,
        "status": published.status,
        "elapsedMs": elapsed_ms,
        "output": published.output,
    });
    if let Some(code) = body.exit_code {
        data["exitCode"] = json!(code);
    }
    if let Some(stdout) = &published.stdout {
        data["stdout"] = json!(stdout);
    }
    if let Some(stderr) = &published.stderr {
        data["stderr"] = json!(stderr);
    }
    Ok(ShellRun {
        command: command.to_owned(),
        status: published.status,
        exit_code: body.exit_code,
        elapsed_ms,
        view,
        data,
    })
}

/// 一条直跑命令的时限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// `bash` 的默认时限（120 秒，受 `call_timeout_secs` 收紧）。命令型 cron 用它。
    Default,
    /// 整个 `[execution] call_timeout_secs`（默认 1800 秒）：TUI 的 `!`。它会转后台，跑满 120 秒
    /// 不该被杀；上限与模型给 `bash` 的 `timeout_secs` 能要到的同一个，热生效。
    CallTimeout,
}

/// 交给 `bash` 的参数。
fn bash_args(command: &str, limit: Limit, execution: &ExecutionConfig) -> Value {
    match limit {
        Limit::Default => json!({ "command": command }),
        Limit::CallTimeout => {
            json!({ "command": command, "timeout_secs": execution.call_timeout_secs })
        }
    }
}

/// `prepare` → `execute`，带兜底时限。失败都折成 `ToolError`（落进 `output.json`，模型看得见）。
async fn run_tool(
    bash: &dyn komo_kernel::traits::Tool,
    args: Value,
    ctx: &ToolContext,
    sink: &mut dyn OutputWriter,
) -> Result<ToolOutput, ToolError> {
    let plan = bash.prepare(args, ctx).await?;
    match tokio::time::timeout(ctx.call_timeout + REAP_GRACE, bash.execute(plan, ctx, sink)).await {
        Ok(result) => result,
        Err(_) => Err(ToolError::Uncertain {
            message: format!(
                "超过活动执行时限 {}s，副作用是否已经发生未知",
                ctx.call_timeout.as_secs()
            ),
        }),
    }
}

/// 送给模型的那条消息：说明这是用户自己跑的命令（不是模型调的），再接工具结果的模型视图。
pub fn model_text(command: &str, view: &str) -> String {
    format!("用户直接运行了 shell 命令 `{command}`（不是你调用的）：\n\n{view}")
}

/// 同上，但命令在 Gateway 正常停机时还没跑完、被终止了（转后台的 `!` 命令不重放）。
fn interrupted_text(command: &str, view: &str) -> String {
    format!(
        "用户直接运行了 shell 命令 `{command}`（不是你调用的），Gateway 停机时它还没跑完，\
         已被终止、不会重跑：\n\n{view}"
    )
}

/// 从 `shell.command` entry 的模型消息里还原"送给模型的输出"（`RunDetail.final_message`）。
pub fn view_of(model_text: &str) -> &str {
    model_text
        .split_once("\n\n")
        .map_or(model_text, |(_, view)| view)
}

/// `POST /v1/sessions/{id}/runs` 的 `shell: true`：跑命令、提交 `shell.command` write。
///
/// 空命令是 `Invalid`；会话不收输入是 `NotOpen`；同一请求键重发返回原 run（**不再跑一遍**；
/// 还在后台跑的返回那张“已转后台”回执），内容不同是请求键冲突。跑过
/// `[execution] detach_after_secs`（0 = 不转）还没完就是 [`ShellReceipt::Detached`]。
pub async fn submit_shell(
    state: &Arc<GatewayState>,
    session: &str,
    request_id: Option<String>,
    command: &str,
) -> Result<ShellReceipt, SessionError> {
    let core = state
        .core()
        .map_err(|error| SessionError::Internal(error.to_string()))?;
    let command = command.trim();
    if command.is_empty() {
        return Err(SessionError::Invalid(
            "`!` 后面要跟一条命令，例如 `!ls -la`".into(),
        ));
    }
    let text = format!("!{command}");
    let hash = ContentHash::of_str(&text).to_string();
    let call = format!("shell-{}", uuid::Uuid::now_v7());
    // 没有请求键的只认得自己。
    let key = request_id.clone().unwrap_or_else(|| call.clone());
    let shell = RunningShell {
        hash: hash.clone(),
        call: call.clone(),
        cancel: CancelToken::new(),
    };
    // 先占位再查落没落账：后台任务落账之后才销号，这个顺序下重发不会两头都落空、把命令再跑一遍。
    if let Claim::Running(existing) = core.shells.claim(session, &key, &shell)? {
        if existing.hash != hash {
            return Err(SessionError::RequestKeyConflict(key));
        }
        return Ok(ShellReceipt::Detached {
            call: existing.call,
            deduplicated: true,
        });
    }
    let release = Release {
        core: Arc::clone(core),
        session: session.to_owned(),
        key: key.clone(),
    };
    if let Some(key) = &request_id
        && let Some(accepted) = core.already_accepted(session, key, &hash).await?
    {
        return Ok(ShellReceipt::Landed(accepted));
    }
    let record = core.session(session).await?;
    super::super::sessions::ensure_open(&record)?;

    let detach_after = state.snapshot().execution.detach_after_secs;
    let job = {
        let state = Arc::clone(state);
        let session = session.to_owned();
        let command = command.to_owned();
        async move {
            let _release = release;
            let landed = run_and_land(
                &state,
                &session,
                record.current_branch,
                &command,
                &shell,
                text,
                request_id,
            )
            .await;
            if let Err(error) = &landed {
                tracing::warn!(%error, session, command, "`!` 命令没能落进会话");
            }
            landed
        }
    };
    // 跑在自己的任务里：请求断了（客户端关了、转后台回执先回去了）命令照样跑完、落账。
    let mut task = tokio::spawn(job);
    let joined = if detach_after == 0 {
        task.await
    } else {
        match tokio::time::timeout(Duration::from_secs(detach_after), &mut task).await {
            Ok(joined) => joined,
            Err(_) => {
                return Ok(ShellReceipt::Detached {
                    call,
                    deduplicated: false,
                });
            }
        }
    };
    joined
        .map_err(|error| SessionError::Internal(format!("`!` 命令的任务崩了：{error}")))?
        .map(ShellReceipt::Landed)
}

/// 跑命令、提交 `shell.command` write（遇忙排进收件箱）。
async fn run_and_land(
    state: &Arc<GatewayState>,
    session: &str,
    conversation: ConversationId,
    command: &str,
    shell: &RunningShell,
    text: String,
    request_id: Option<String>,
) -> Result<Accepted, SessionError> {
    let ran = run_command(
        state,
        conversation,
        command,
        &shell.call,
        &shell.cancel,
        Limit::CallTimeout,
    )
    .await?;
    let core = state
        .core()
        .map_err(|error| SessionError::Internal(error.to_string()))?;
    // 只有停机会举这面旗（[`RunningShells::shutdown`]）。
    let interrupted = shell.cancel.is_cancelled();
    let now_ms = (core.clock.now().unix_timestamp_nanos() / 1_000_000) as i64;
    let mut data = ran.data.clone();
    data["inputHash"] = json!(shell.hash);
    // TUI 按它撤掉自己那条待确认的行（`MessageUser::shell.request_key`）。
    if let Some(key) = &request_id {
        data["requestKey"] = json!(key);
    }
    if interrupted {
        data["interrupted"] = json!(true);
    }
    let message = if interrupted {
        interrupted_text(command, &ran.view)
    } else {
        model_text(command, &ran.view)
    };
    let entry = WriteDraft {
        kind: SHELL_ENTRY.to_owned(),
        model: Some(vec![Message::user(message, now_ms)]),
        data: Some(data),
        head: None,
        edits: None,
    };
    let draft = SubmissionDraft::Write { entry, request_id };
    core.land(
        session,
        draft,
        Landing {
            hash: shell.hash.clone(),
            text,
            reject_when_busy: false,
            model: None,
            interject: false,
        },
    )
    .await
}

impl DurableCore {
    /// 一条 `shell.command` write 的 entry → 它的输出（`run_detail` 用）。
    pub(in crate::service::durable) fn shell_view(
        entry: &komo_kernel::durable::EntryRecord,
    ) -> Option<String> {
        let text = entry
            .model
            .as_ref()?
            .iter()
            .find_map(super::super::sessions::runs::user_text_of)?;
        Some(view_of(&text).to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bang_command_gets_the_whole_call_timeout_and_a_cron_command_the_bash_default() {
        let execution = ExecutionConfig {
            call_timeout_secs: 2400,
            ..ExecutionConfig::default()
        };
        assert_eq!(
            bash_args("make", Limit::CallTimeout, &execution),
            json!({ "command": "make", "timeout_secs": 2400 })
        );
        assert_eq!(
            bash_args("make", Limit::Default, &execution),
            json!({ "command": "make" }),
            "命令型 cron 不变"
        );
    }
}
