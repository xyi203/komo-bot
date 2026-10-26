//! `!` 命令直跑（`docs/storage-durable-d5.md` 决定 9）：gateway 直接用 `bash` 工具跑一条命令，
//! **不建 harness run**、不发模型请求；完整输出落 blob，再提交一条 `shell.command` write
//! （带 `model`：下一轮模型看得见这条命令与它的输出）。不做崩溃重放：进程在命令跑到一半时没了，
//! 命令就是没跑完——没有 submission 记下它，也不会被重新跑。
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

use std::sync::Arc;

use komo_kernel::durable::{ConversationId, Message};
use komo_kernel::projection::{ToolResultFacts, project_views, select};
use komo_kernel::traits::{OutputWriter, ToolOutputStore};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::{AttemptId, RunId, ToolCallId};
use komo_kernel::types::refs::{AttemptRef, ToolResultBody, ToolResultStatus};
use komo_kernel::types::tool::{CancelToken, InterjectSignal, ToolContext, ToolError, ToolOutput};
use komo_runtime::durable::harness::{SubmissionDraft, WriteDraft};
use komo_store::{FileToolOutputStore, SessionPaths};
use serde_json::{Value, json};

use super::DurableCore;
use super::hooks::{self, env, tools};
use super::sessions::SessionError;
use super::submit::{Accepted, Landing};
use crate::service::state::GatewayState;

/// `!` 命令放进会话的 entry 的 kind（`docs/storage-durable-d5.md` §2：gateway 写的 `shell.command`）。
pub const SHELL_ENTRY: &str = "shell.command";

/// 兜底计时器比 `call_timeout` 多给的一段：工具自己守时限，终止进程组与写完输出还要时间
/// （同 harness 的 `REAP_GRACE`）。
const REAP_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

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
/// `call` 是输出目录里 `call` 一级的名字（不同命令不同）。
pub async fn run_command(
    state: &Arc<GatewayState>,
    conversation: ConversationId,
    command: &str,
    call: &str,
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
        cancel: CancelToken::new(),
        call_timeout: tool_env.call_timeout,
        // 前台等着：不转后台（没有人会收那条"已转后台"），也没有"禁长 sleep"这条对话会话的规矩。
        detach_after: None,
        interjected: InterjectSignal::new(),
        conversation: false,
    };

    let mut writer = store
        .begin(&attempt)
        .await
        .map_err(|error| SessionError::Internal(format!("输出目录建不出来：{error}")))?;
    let started = std::time::Instant::now();
    let outcome = run_tool(&*bash, command, &ctx, writer.as_mut()).await;
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

/// `prepare` → `execute`，带兜底时限。失败都折成 `ToolError`（落进 `output.json`，模型看得见）。
async fn run_tool(
    bash: &dyn komo_kernel::traits::Tool,
    command: &str,
    ctx: &ToolContext,
    sink: &mut dyn OutputWriter,
) -> Result<ToolOutput, ToolError> {
    let plan = bash.prepare(json!({ "command": command }), ctx).await?;
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

/// 从 `shell.command` entry 的模型消息里还原"送给模型的输出"（`RunDetail.final_message`）。
pub fn view_of(model_text: &str) -> &str {
    model_text
        .split_once("\n\n")
        .map_or(model_text, |(_, view)| view)
}

/// `POST /v1/sessions/{id}/runs` 的 `shell: true`：跑命令、提交 `shell.command` write。
///
/// 空命令是 `Invalid`；会话不收输入是 `NotOpen`；同一请求键重发返回原 run（**不再跑一遍**），
/// 内容不同是请求键冲突。
pub async fn submit_shell(
    state: &Arc<GatewayState>,
    session: &str,
    request_id: Option<String>,
    command: &str,
) -> Result<Accepted, SessionError> {
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
    if let Some(key) = &request_id
        && let Some(accepted) = core.already_accepted(session, key, &hash).await?
    {
        return Ok(accepted);
    }
    let record = core.session(session).await?;
    super::sessions::ensure_open(&record)?;

    let call = format!("shell-{}", uuid::Uuid::now_v7());
    let ran = run_command(state, record.current_branch, command, &call).await?;
    let now_ms = (core.clock.now().unix_timestamp_nanos() / 1_000_000) as i64;
    let mut data = ran.data.clone();
    data["inputHash"] = json!(hash);
    let entry = WriteDraft {
        kind: SHELL_ENTRY.to_owned(),
        model: Some(vec![Message::user(model_text(command, &ran.view), now_ms)]),
        data: Some(data),
        head: None,
        edits: None,
    };
    let draft = SubmissionDraft::Write { entry, request_id };
    core.land(
        session,
        draft,
        Landing {
            hash,
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
    pub(super) fn shell_view(entry: &komo_kernel::durable::EntryRecord) -> Option<String> {
        let text = entry
            .model
            .as_ref()?
            .iter()
            .find_map(super::runs::user_text_of)?;
        Some(view_of(&text).to_owned())
    }
}
