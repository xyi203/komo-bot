//! codemode 在执行器里的那一半（`docs/codemode.md` §3、§5）：跑脚本，回答脚本里的工具调用。
//!
//! 外层是一次普通调用（Policy、`start_call`、输出落盘都在 `execute_authorized`）；这里
//! 只替换"执行"那一步。脚本里的每一次调用走 [`ToolExecutor::nested_call`]：能力面 →
//! `prepare` → 只读 → Policy 直接 Allow → 执行。内层调用不单独落账，只在外层结果里留摘要
//! ——成立的前提是它们全是只读的，整段重跑没有副作用。

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use komo_kernel::policy::PolicyDecision;
use komo_kernel::traits::{OutputWriter, StoreError};
use komo_kernel::types::plan::{ApprovedPlan, Operation};
use komo_kernel::types::refs::{AttemptRef, ToolResultStatus};
use komo_kernel::types::tool::{ToolContext, ToolError, ToolOutput};
use serde::Serialize;

use super::{CallEnv, ToolExecutor};
use crate::codemode::OUTPUT_LIMIT;
use crate::policy::DecisionEnv;

/// 脚本里调不了的：编排操作与脚本自己。
const NOT_IN_SCRIPTS: &[&str] = &["codemode", "delegate", "dispatch", "follow"];

/// 摘要里每次调用的参数最多留这么长。
const ARGS_PREVIEW: usize = 512;

/// 一次内层调用的摘要（外层 `output.json` 的 `result.calls`）。
#[derive(Debug, Clone, Serialize)]
struct NestedCall {
    tool: String,
    args: String,
    ok: bool,
    elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl ToolExecutor {
    pub(super) async fn run_codemode(
        &self,
        code: &str,
        env: &CallEnv,
        ctx: &ToolContext,
        sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let Some(sandbox) = self.codemode.as_ref() else {
            return Err(ToolError::Failed {
                message: "这台 Gateway 没有可用的 codemode 沙箱".into(),
            });
        };
        let names: Vec<String> = env
            .surface
            .names()
            .iter()
            .filter(|name| !NOT_IN_SCRIPTS.contains(&name.as_str()))
            .cloned()
            .collect();
        let calls: Arc<Mutex<Vec<NestedCall>>> = Arc::default();
        let outcome = sandbox
            .run(code, &names, env.call_timeout, |name, args| {
                let calls = Arc::clone(&calls);
                async move {
                    let started = Instant::now();
                    let preview = truncate(&args.to_string(), ARGS_PREVIEW);
                    let result = self.nested_call(&name, args, env, ctx).await;
                    calls.lock().expect("调用摘要").push(NestedCall {
                        tool: name,
                        args: preview,
                        ok: result.is_ok(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        error: result.as_ref().err().cloned(),
                    });
                    result
                }
            })
            .await;
        let calls = std::mem::take(&mut *calls.lock().expect("调用摘要"));

        let mut text = join_outputs(&outcome.outputs);
        if !outcome.console.is_empty() {
            text.push_str(&format!("\n<console>\n{}</console>", outcome.console));
        }
        if let Some(error) = &outcome.error {
            text.push_str(&format!("\n脚本出错：{error}"));
        }
        if !calls.is_empty() {
            text.push_str(&format!("\n（{}）", summary(&calls)));
        }
        let text = truncate(text.trim_start(), OUTPUT_LIMIT);
        sink.write_stdout(text.as_bytes())
            .await
            .map_err(|error| ToolError::Failed {
                message: error.to_string(),
            })?;
        Ok(ToolOutput {
            status: if outcome.error.is_some() {
                ToolResultStatus::Failed
            } else {
                ToolResultStatus::Completed
            },
            result: serde_json::json!({ "calls": calls, "error": outcome.error }),
            exit_code: None,
            artifacts: vec![],
            preview: Some(text),
        })
    }

    /// 脚本里的一次工具调用（`docs/codemode.md` §5）。错误就是交给脚本的 `ToolError` 正文。
    async fn nested_call(
        &self,
        name: &str,
        args: serde_json::Value,
        env: &CallEnv,
        ctx: &ToolContext,
    ) -> Result<serde_json::Value, String> {
        if NOT_IN_SCRIPTS.contains(&name) || !env.surface.allows(name) {
            return Err(format!("脚本里不能调 {name}"));
        }
        let tool = self
            .tools
            .get(name)
            .cloned()
            .ok_or_else(|| format!("没有名为 {name} 的工具"))?;
        let plan = tool
            .prepare(args, ctx)
            .await
            .map_err(|error| error.to_string())?;
        if !plan.operation.is_read_only() || matches!(plan.operation, Operation::Codemode { .. }) {
            return Err(format!(
                "{name} 不是只读的，脚本里只能调只读工具；请在脚本外直接调用"
            ));
        }
        // 不查授权表：脚本里只认配置直接放行的那些，授权的消费要落账，内层调用不落账。
        let decision = self.policy.decide(
            &plan,
            &DecisionEnv {
                grants: &[],
                principal: env.principal.as_ref(),
                roots: &env.roots,
                now: self.clock.now(),
            },
        );
        let proof = match decision {
            PolicyDecision::Ask { reason, .. } => {
                return Err(format!(
                    "这一步要审批（{reason}），脚本里不能停下来等人；请在脚本外直接调用 {name}"
                ));
            }
            PolicyDecision::Deny { reason } => return Err(format!("被拒绝：{reason}")),
            allow => allow.into_proof().expect("Allow 换得出 Proof"),
        };
        let mut capture = Capture::new(AttemptRef {
            session: ctx.session.clone(),
            run: ctx.run.clone(),
            call: ctx.call.clone(),
            attempt: ctx.attempt.clone(),
        });
        let output = tool
            .execute(ApprovedPlan::new(plan, proof), ctx, &mut capture)
            .await
            .map_err(|error| error.to_string())?;
        let text = match output.result.get("text").and_then(|text| text.as_str()) {
            Some(text) => text.to_string(),
            None if !capture.bytes.is_empty() => String::from_utf8_lossy(&capture.bytes).into(),
            None => output.preview.clone().unwrap_or_default(),
        };
        Ok(serde_json::json!({
            "status": output.status,
            "text": text,
            "result": output.result,
        }))
    }
}

fn summary(calls: &[NestedCall]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for call in calls {
        match counts.iter_mut().find(|(tool, _)| *tool == call.tool) {
            Some((_, count)) => *count += 1,
            None => counts.push((&call.tool, 1)),
        }
    }
    let failed = calls.iter().filter(|call| !call.ok).count();
    let listed = counts
        .iter()
        .map(|(tool, count)| format!("{tool}×{count}"))
        .collect::<Vec<_>>()
        .join("、");
    match failed {
        0 => format!("调用了 {} 次工具：{listed}", calls.len()),
        n => format!("调用了 {} 次工具：{listed}；{n} 次失败", calls.len()),
    }
}

/// 多段 `text()` 拼起来：两段以上时每段前加 `==> text N/M <==`（学 pi 1.1.0）——直接用换行
/// 拼，模型分不清一段多行输出和两段输出的边界。只有一段时原样给，不多一个字。
fn join_outputs(outputs: &[String]) -> String {
    if outputs.len() <= 1 {
        return outputs.join("\n");
    }
    let total = outputs.len();
    outputs
        .iter()
        .enumerate()
        .map(|(index, output)| format!("==> text {}/{total} <==\n{output}", index + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…（截断）", &text[..end])
}

/// 内层调用的输出收进内存（上限同脚本输出）。
struct Capture {
    attempt: AttemptRef,
    bytes: Vec<u8>,
}

impl Capture {
    fn new(attempt: AttemptRef) -> Self {
        Self {
            attempt,
            bytes: Vec::new(),
        }
    }
}

#[async_trait]
impl OutputWriter for Capture {
    async fn write_stdout(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        let room = OUTPUT_LIMIT.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&chunk[..chunk.len().min(room)]);
        Ok(())
    }

    async fn write_stderr(&mut self, _chunk: &[u8]) -> Result<(), StoreError> {
        Ok(())
    }

    fn attempt(&self) -> &AttemptRef {
        &self.attempt
    }

    fn bytes_written(&self) -> u64 {
        self.bytes.len() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::join_outputs;

    #[test]
    fn a_single_output_is_left_as_is() {
        assert_eq!(join_outputs(&["a\nb".to_string()]), "a\nb");
        assert_eq!(join_outputs(&[]), "");
    }

    #[test]
    fn several_outputs_are_headed_with_their_position() {
        let joined = join_outputs(&["a\nb".to_string(), "c".to_string()]);
        assert_eq!(joined, "==> text 1/2 <==\na\nb\n==> text 2/2 <==\nc");
    }
}
