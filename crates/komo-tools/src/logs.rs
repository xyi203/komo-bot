//! The `logs` tool: let the agent read komo's own tracing log during a
//! conversation, so "why did that tool fail?" can be answered from the actual
//! `tool` span instead of a guess.
//!
//! Deliberately not the `read` tool with a path: the live file depends on which
//! process is running the turn (the chat TUI writes `chat-tui.log`, the gateway a
//! daily-rotated `gateway.YYYY-MM-DD.log`), and that resolution lives in
//! `infra::logs` — shared with `komo logs`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::fs_common;
use komo_core::domain::{
    context::ToolContext,
    tool::{Tool, ToolError, ToolOutput, parse_args},
};
use komo_infra::logs;

/// Lines returned when the caller doesn't say.
const DEFAULT_LINES: usize = 120;
/// Ceiling on one call. Log lines are wide; the executor's result cap would cut
/// the tail off anyway, and losing the *newest* lines is the wrong failure.
const MAX_LINES: usize = 500;
/// A single line longer than this is truncated — one verbatim tool result (at
/// `KOMO_LOG=debug`) would otherwise consume the whole budget.
const MAX_LINE_CHARS: usize = 1_000;

#[derive(Deserialize)]
struct LogsArgs {
    /// How many (matching) lines from the end of the file to return.
    #[serde(default)]
    lines: Option<usize>,
    /// Keep only lines containing this substring (case-insensitive).
    #[serde(default)]
    contains: Option<String>,
    /// `auto` (default) = the log this process writes, `chat` / `gateway` pick a
    /// specific one.
    #[serde(default)]
    source: Option<String>,
}

pub struct LogsTool;

#[async_trait]
impl Tool for LogsTool {
    fn name(&self) -> &'static str {
        "logs"
    }

    fn description(&self) -> &'static str {
        "Read the tail of komo's own runtime log: turn spans, tool outcomes, \
         channel and sweep activity. At the default `info` level a tool's full \
         result is not logged, only its name, outcome and duration."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "lines": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_LINES,
                    "description": format!("Newest matching lines to return (default {DEFAULT_LINES})."),
                },
                "contains": {
                    "type": "string",
                    "description": "Keep only lines containing this substring (case-insensitive).",
                },
                "source": {
                    "type": "string",
                    "enum": ["auto", "chat", "gateway"],
                    "description": "Which log to read; defaults to this process's own.",
                },
            },
            "required": []
        })
    }

    /// Read-only: safe to retry.
    fn idempotent(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: LogsArgs = parse_args(&input)?;
        let lines = args.lines.unwrap_or(DEFAULT_LINES).clamp(1, MAX_LINES);
        let source = args.source.as_deref().unwrap_or("auto");
        let path = resolve(source)?;

        // Same file-read gate as `read`/`grep`: never prompts (reads are
        // `Risk::Safe`), but a path-scoped `category = "file", access = "read"`
        // deny rule still applies — this tool must not be the way around it.
        if let Some(refusal) = fs_common::allow_read(ctx, &path).await {
            return Ok(ToolOutput::text(refusal));
        }

        let tail = logs::tail(&path, lines, args.contains.as_deref())
            .map_err(|e| ToolError::Failed(anyhow::anyhow!("{}: {e}", path.display())))?;

        Ok(ToolOutput::text(render(&path, &tail))
            .with_title(format!("logs {}", tail.lines.len()))
            .with_structured(json!({
                "file": path.display().to_string(),
                "returned": tail.lines.len(),
                "matched": tail.matched,
            })))
    }
}

/// The model-facing view: which file, how much matched, then the lines. The
/// header matters — "12 of 4000 matched" and "nothing matched" are different
/// findings, and without it an empty tail looks like a broken tool.
fn render(path: &std::path::Path, tail: &logs::Tail) -> String {
    let mut text = format!(
        "{} — {} matching line(s), showing the last {}\n",
        path.display(),
        tail.matched,
        tail.lines.len()
    );
    if tail.lines.is_empty() {
        text.push_str("(nothing matched; try a wider filter or more lines)\n");
    }
    for line in &tail.lines {
        let line = line.trim_end_matches('\n');
        if line.chars().count() > MAX_LINE_CHARS {
            let kept: String = line.chars().take(MAX_LINE_CHARS).collect();
            text.push_str(&kept);
            text.push_str("…\n");
        } else {
            text.push_str(line);
            text.push('\n');
        }
    }
    text
}

/// Which file `source` names. An unknown value is [`ToolError::InvalidInput`]
/// (the model can retry with a valid one); a source that has never been written
/// is a plain failure with the path, which reads better than an empty result.
fn resolve(source: &str) -> Result<std::path::PathBuf, ToolError> {
    let dir = logs::dir();
    let path = match source {
        // The gateway's file rotates daily, so `auto` finds it by name rather
        // than pinning it at startup; only the TUI registers a fixed path.
        "auto" => logs::active()
            .map(std::path::Path::to_path_buf)
            .or_else(|| logs::latest_gateway_log(&dir))
            .unwrap_or_else(logs::chat_log),
        "chat" => logs::chat_log(),
        "gateway" => logs::latest_gateway_log(&dir).unwrap_or_else(|| dir.join("gateway.err.log")),
        other => {
            return Err(ToolError::InvalidInput(format!(
                "unknown source `{other}` — use auto, chat, or gateway"
            )));
        }
    };
    if !path.exists() {
        return Err(ToolError::Failed(anyhow::anyhow!(
            "no log file at {} — this process may be logging to stderr instead \
             (only the chat TUI and the gateway write log files)",
            path.display()
        )));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::detached_ctx;

    #[tokio::test]
    async fn unknown_source_is_recoverable_invalid_input() {
        let err = LogsTool
            .call(json!({"source": "syslog"}), &detached_ctx("cli:test"))
            .await
            .expect_err("unknown source must be refused");
        assert!(
            matches!(err, ToolError::InvalidInput(m) if m.contains("syslog")),
            "the model should get a rewrite-the-arguments error"
        );
    }

    #[test]
    fn render_reports_the_match_count_and_truncates_wide_lines() {
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        let tail = logs::Tail {
            lines: vec!["short line\n".to_string(), format!("{long}\n")],
            end: 0,
            matched: 4_000,
        };
        let text = render(std::path::Path::new("/tmp/gateway.2026-07-31.log"), &tail);
        assert!(text.starts_with(
            "/tmp/gateway.2026-07-31.log — 4000 matching line(s), showing the last 2\n"
        ));
        assert!(text.contains("short line\n"));
        let rendered_long = text.lines().last().unwrap();
        assert_eq!(
            rendered_long.chars().count(),
            MAX_LINE_CHARS + 1,
            "capped + ellipsis"
        );
        assert!(rendered_long.ends_with('…'));
    }

    #[test]
    fn render_says_so_when_nothing_matched() {
        let tail = logs::Tail {
            lines: Vec::new(),
            end: 0,
            matched: 0,
        };
        let text = render(std::path::Path::new("/tmp/chat-tui.log"), &tail);
        assert!(text.contains("nothing matched"), "{text}");
    }

    #[tokio::test]
    async fn a_missing_log_file_explains_itself() {
        // `chat` is a fixed name, so this is deterministic wherever KOMO_HOME
        // points: either the file exists (and we get output) or the error names
        // the path instead of returning an empty tail.
        match LogsTool
            .call(json!({"source": "chat"}), &detached_ctx("cli:test"))
            .await
        {
            Ok(out) => assert!(out.text.contains("chat-tui.log")),
            Err(ToolError::Failed(e)) => {
                let msg = format!("{e:#}");
                assert!(msg.contains("chat-tui.log"), "error names the path: {msg}");
            }
            Err(other) => panic!("unexpected error kind: {other}"),
        }
    }

    #[test]
    fn the_model_facing_text_stays_short() {
        crate::test_support::assert_model_text_budget(&LogsTool);
    }
}
