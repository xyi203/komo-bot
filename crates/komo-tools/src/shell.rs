use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use komo_core::domain::{
    approval::{ActionRef, ApprovalRequest, Decision},
    cancel::Cancelled,
    context::ToolContext,
    tool::{Tool, ToolError, ToolOutput, parse_args},
    workspace::Workspace,
};

/// Why the command stopped being waited on. Both outcomes kill the process
/// group; they differ in what the caller is told.
enum Interrupt {
    /// The command's own `timeout` elapsed — reported to the model, which can
    /// retry with a bigger one.
    Timeout,
    /// The user stopped the turn. Nothing will read a reply, so this ends the
    /// call as an error the ledger records.
    Cancelled,
}

/// Command substrings treated as high-risk. Matching commands are flagged as
/// dangerous in the approval prompt.
const DANGEROUS_PATTERNS: &[&str] = &[
    "rm ",
    "rm -",
    "rmdir",
    "unlink",
    "git push",
    "git reset --hard",
    "git clean",
    "git branch -d",
    "git checkout --",
    "dd ",
    "mkfs",
    "sudo ",
    "shutdown",
    "reboot",
    "kill ",
    "killall",
    "chmod ",
    "chown ",
    "truncate",
    "> /dev/",
    "mv ",
    ":(){",
];

/// Commands that are never run, even with user approval (hermes calls this the
/// "hardline floor"): the blast radius is the whole machine, not the workspace.
const HARDLINE_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -fr /",
    "mkfs",
    "dd if=/dev/zero of=/dev/",
    "of=/dev/sd",
    "of=/dev/disk",
    ":(){",
    "shutdown",
    "reboot",
    "halt",
];

/// True if `pattern` occurs in `haystack` (already lowercased) at a command
/// boundary, not buried inside a larger alphanumeric word. A naive `contains`
/// flags `terraform apply` and `kill -TERM 1` as the `rm ` pattern, because
/// "rm " is a substring of "terrafo*rm* " and "-te*rm* 1". We require the char
/// before the match to be a non-alphanumeric (or start), and — when the pattern
/// ends in a letter/digit — the char after it likewise, so the pattern lines up
/// with a real token rather than the middle of one.
fn matches_at_boundary(haystack: &str, pattern: &str) -> bool {
    let bytes = haystack.as_bytes();
    let pat = pattern.as_bytes();
    let pattern_ends_alnum = pat.last().is_some_and(u8::is_ascii_alphanumeric);
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(pattern) {
        let at = from + rel;
        let before_ok = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
        let after = at + pat.len();
        let after_ok =
            !pattern_ends_alnum || after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        // The matched pattern starts on an ASCII byte, so `at + 1` is a valid
        // char boundary to resume the scan from.
        from = at + 1;
    }
    false
}

fn dangerous_pattern(command: &str) -> Option<&'static str> {
    let lc = command.to_lowercase();
    DANGEROUS_PATTERNS
        .iter()
        .copied()
        .find(|p| matches_at_boundary(&lc, p))
}

fn hardline_pattern(command: &str) -> Option<&'static str> {
    let lc = command.to_lowercase();
    HARDLINE_PATTERNS
        .iter()
        .copied()
        .find(|p| matches_at_boundary(&lc, p))
}

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
    /// Wall-clock budget in milliseconds. The model asks for more when it knows
    /// the command is slow (a build, a test run) rather than losing the work to a
    /// fixed ceiling.
    #[serde(default)]
    timeout: Option<u64>,
    /// Working directory, relative to the workspace root (default: the root).
    #[serde(default)]
    workdir: Option<String>,
}

/// Default command budget, matching opencode v2's `bash`.
const DEFAULT_TIMEOUT_MS: u64 = 2 * 60 * 1_000;
/// Ceiling on what the model may ask for.
const MAX_TIMEOUT_MS: u64 = 10 * 60 * 1_000;

/// Markers that introduce a secret value as `marker=<secret>` (case-insensitive).
const SECRET_KEY_MARKERS: &[&str] = &[
    "api_key=",
    "apikey=",
    "api-key=",
    "token=",
    "secret=",
    "password=",
    "passwd=",
    "pwd=",
    "access_key=",
    "auth=",
];

/// Flags whose *following* token is a secret (`--password hunter2`).
const SECRET_FLAGS: &[&str] = &["--password", "--token", "--api-key", "--secret", "-p"];

/// Upper bound on how many bytes of stdout/stderr each stream is read into
/// memory. Well above the LLM result cap (which truncates the model-facing text
/// anyway), so it never clips useful output — it only stops a command that
/// spews unbounded output (`cat` a huge file, `yes`) from OOMing the gateway.
/// Reading stops at the cap and the child is killed (`kill_on_drop`).
const MAX_STREAM_BYTES: u64 = 256 * 1024;

/// A token that "looks like" an opaque credential: long and a single run of
/// url-safe-ish characters with no shell punctuation.
fn looks_like_secret(token: &str) -> bool {
    token.len() >= 24
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '='))
        && token.chars().any(|c| c.is_ascii_digit())
        && token.chars().any(|c| c.is_ascii_alphabetic())
}

/// Best-effort scrub of secret-looking substrings from a shell command before it
/// is written to the run ledger. Heuristic, dependency-free, whitespace-tokenized:
/// covers `key=value`, `Bearer <tok>`, `--password <tok>`, and high-entropy
/// tokens. The command structure stays readable; only the secret is replaced.
fn redact_secrets(command: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut scrub_next = false;
    for raw in command.split_whitespace() {
        if scrub_next {
            out.push("***".to_string());
            scrub_next = false;
            continue;
        }
        let lower = raw.to_lowercase();
        if lower == "bearer" || SECRET_FLAGS.contains(&lower.as_str()) {
            out.push(raw.to_string());
            scrub_next = true;
            continue;
        }
        if let Some(marker) = SECRET_KEY_MARKERS.iter().find(|m| lower.starts_with(**m)) {
            // Preserve the original-case key prefix, drop the value.
            out.push(format!("{}***", &raw[..marker.len()]));
            continue;
        }
        if looks_like_secret(raw) {
            out.push("***".to_string());
            continue;
        }
        out.push(raw.to_string());
    }
    out.join(" ")
}

/// Runs a shell command via `sh -c`, gated behind an [`Approver`]. Dangerous
/// commands (deletes, `git push`, `sudo`, ...) are flagged prominently. Runs
/// with the working directory set to the workspace root.
pub struct ShellTool {
    workspace: Arc<Workspace>,
}

impl ShellTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        "Run a shell command on the local machine via `sh -c` and return its \
         combined stdout/stderr. Safe (read-only) commands run without a \
         prompt; destructive commands require an explicit dangerous-action \
         confirmation, and a few catastrophic ones are always refused."
    }

    /// The caller may ask for up to [`MAX_TIMEOUT_MS`]; the executor's clock has
    /// to sit *above* that, or a legitimate long command would be aborted with an
    /// opaque error instead of this tool's "retry with a bigger timeout". The
    /// slack also covers a human sitting on the approval prompt.
    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(
            std::time::Duration::from_millis(MAX_TIMEOUT_MS)
                + komo_core::domain::tool::APPROVAL_BOUND,
        )
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to run, e.g. `ls -la`."
                },
                "timeout": {
                    "type": "integer",
                    "description": format!(
                        "Milliseconds to allow before the command (and anything it \
                         started) is killed. Default {DEFAULT_TIMEOUT_MS}, maximum \
                         {MAX_TIMEOUT_MS}. Raise it for builds and test runs."
                    )
                },
                "workdir": {
                    "type": "string",
                    "description": "Directory to run in, relative to the workspace root. Defaults to the root."
                }
            },
            "required": ["command"]
        })
    }

    /// Scrub secret-looking substrings from the command before it lands in the
    /// run ledger (the command itself is kept for audit; only secrets go).
    fn redact_args(&self, args: &str) -> String {
        match serde_json::from_str::<serde_json::Value>(args) {
            Ok(mut v) => {
                if let Some(cmd) = v.get("command").and_then(|c| c.as_str()) {
                    v["command"] = serde_json::json!(redact_secrets(cmd));
                }
                v.to_string()
            }
            Err(_) => "<shell args redacted>".to_string(),
        }
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ShellArgs = parse_args(&input)?;

        // Hardline floor: catastrophic commands are refused outright — no
        // approval can unlock them.
        if let Some(pattern) = hardline_pattern(&args.command) {
            return Ok(ToolOutput::text(format!(
                "Command refused: matched hardline pattern `{pattern}`. \
                 This command is never run, even with approval. Do not retry it."
            )));
        }

        // Approval gate (hermes-style): commands matching a dangerous pattern
        // prompt the user; everything else is `Risk::Safe` and an interactive
        // approver lets it through without asking.
        let summary = format!("run shell command: {}", args.command);
        let action = ActionRef::Shell {
            command: args.command.clone(),
        };
        let request = match dangerous_pattern(&args.command) {
            Some(pattern) => ApprovalRequest::dangerous(
                summary,
                format!("matched dangerous pattern `{pattern}`"),
            )
            .with_scope_key(format!("shell:{pattern}"))
            .with_action(action),
            None => ApprovalRequest::safe(summary).with_action(action),
        };
        // A denial may carry the user's reason ("use trash instead of rm") —
        // hand it back so the next round is a corrected command, not a retry.
        if let Decision::Deny { feedback } = ctx.decide(&request).await {
            return Ok(ToolOutput::text(match feedback {
                Some(reason) => format!(
                    "Command rejected by the user; nothing was run. \
                     They said: {reason}\nAct on that instead of retrying the same command."
                ),
                None => "Command rejected by user; nothing was run.".to_string(),
            }));
        }

        let workspace = crate::fs_common::effective(&self.workspace, ctx);
        let cwd = match &args.workdir {
            Some(dir) => {
                let path = workspace
                    .resolve_contained(std::path::Path::new(dir))
                    .ok_or_else(|| {
                        ToolError::Denied(format!(
                            "workdir `{dir}` is outside the workspace and was blocked."
                        ))
                    })?;
                if !tokio::fs::metadata(&path)
                    .await
                    .map(|m| m.is_dir())
                    .unwrap_or(false)
                {
                    return Err(ToolError::InvalidInput(format!(
                        "workdir `{dir}` is not a directory."
                    )));
                }
                Some(path)
            }
            None => workspace.roots().first().cloned(),
        };

        let timeout = std::time::Duration::from_millis(
            args.timeout
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );

        let plan = CommandPlan {
            command: args.command.clone(),
            cwd,
            timeout,
        };

        match plan.run(ctx).await {
            // The turn is already ending, so nothing will read a reply — the
            // point of returning an error is the ledger, which records this step
            // with the same wording as the run's own cancellation.
            Ran::Cancelled => Err(ToolError::Failed(Cancelled.into())),
            Ran::Broken(error) => Err(ToolError::Failed(anyhow::anyhow!(error))),
            outcome => Ok(plan.render(&outcome)),
        }
    }
}

/// One command, resolved: everything needed to run it and nothing borrowed from
/// the turn.
struct CommandPlan {
    command: String,
    cwd: Option<std::path::PathBuf>,
    timeout: std::time::Duration,
}

/// How the command stopped.
enum Ran {
    Exited {
        out: Vec<u8>,
        err: Vec<u8>,
        code: Option<i32>,
    },
    /// Its own `timeout` elapsed and the process group was killed.
    TimedOut,
    /// The user stopped the turn.
    Cancelled,
    /// Never ran, or could not be awaited.
    Broken(String),
}

impl CommandPlan {
    /// Run it, racing the turn's cancellation.
    async fn run(&self, ctx: &ToolContext) -> Ran {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(&self.command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // If the executor's wall-clock timeout aborts the task awaiting this
            // command, dropping the `Child` must kill the process — otherwise
            // `sh` (and its children) would be orphaned and keep running.
            .kill_on_drop(true);
        // Own a process *group*, so a timeout can kill the whole tree. Without
        // this, killing `sh` leaves its children (`sleep`, a dev server, a
        // compiler) running with the pipe still open.
        #[cfg(unix)]
        cmd.process_group(0);
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => return Ran::Broken(format!("failed to spawn command: {e}")),
        };
        let pgid = child.id().map(|id| id as i32);

        // Read both streams concurrently, each bounded to MAX_STREAM_BYTES, so a
        // command emitting unbounded output can't buffer the whole thing into
        // memory and OOM the gateway. `stdin(null)` above means a command that
        // reads stdin sees EOF instead of blocking forever waiting for input.
        let mut out_pipe = child.stdout.take();
        let mut err_pipe = child.stderr.take();
        let read_out = async {
            let mut buf = Vec::new();
            if let Some(p) = out_pipe.as_mut() {
                let _ = p.take(MAX_STREAM_BYTES).read_to_end(&mut buf).await;
            }
            buf
        };
        let read_err = async {
            let mut buf = Vec::new();
            if let Some(p) = err_pipe.as_mut() {
                let _ = p.take(MAX_STREAM_BYTES).read_to_end(&mut buf).await;
            }
            buf
        };
        // Race the command against its budget. Reading the pipes is part of the
        // race: a command that writes nothing and hangs must time out too.
        let run = async {
            let (out, err) = tokio::join!(read_out, read_err);
            let status = child.wait().await;
            (out, err, status)
        };
        // Two ways to lose the race, and both kill the group: the command's own
        // budget elapsed, or the user asked to stop the turn. `shell` is the tool
        // that most needs the second one — interrupting a ten-minute build should
        // actually end the build, not just stop waiting for it.
        let outcome = tokio::select! {
            r = tokio::time::timeout(self.timeout, run) => r.map_err(|_| Interrupt::Timeout),
            _ = ctx.cancelled() => Err(Interrupt::Cancelled),
        };

        match outcome {
            Ok((out, err, status)) => match status {
                Ok(status) => Ran::Exited {
                    out,
                    err,
                    code: status.code(),
                },
                Err(e) => Ran::Broken(format!("failed to await command: {e}")),
            },
            // Kill the whole group: `sh` alone would leave its children running
            // (and holding the pipes) forever.
            Err(Interrupt::Timeout) => {
                kill_group(pgid);
                Ran::TimedOut
            }
            Err(Interrupt::Cancelled) => {
                kill_group(pgid);
                Ran::Cancelled
            }
        }
    }

    /// The model-facing result of a finished command.
    fn render(&self, outcome: &Ran) -> ToolOutput {
        let clipped = |raw: &[u8]| (raw.len() as u64) >= MAX_STREAM_BYTES;
        match outcome {
            Ran::TimedOut => ToolOutput::text(format!(
                "Command timed out after {} ms and was killed (along with any \
                 processes it started). Retry with a larger `timeout` if it \
                 legitimately takes longer — the maximum is {MAX_TIMEOUT_MS} ms.",
                self.timeout.as_millis()
            ))
            .with_title(format!("shell (timed out): {}", self.command))
            // `structured` carries the machine-readable outcome (`exit`,
            // `truncated`, `timeout`) so a UI/ledger reader doesn't have to
            // parse the prose.
            .with_structured(json!({ "exit": null, "truncated": false, "timeout": true })),
            Ran::Exited { out, err, code } => {
                let stdout = String::from_utf8_lossy(out);
                let stderr = String::from_utf8_lossy(err);
                let status_text = code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string());
                let mut result = format!("exit status: {status_text}");
                if !stdout.trim().is_empty() {
                    result.push_str(&format!("\n--- stdout ---\n{}", stdout.trim_end()));
                    if clipped(out) {
                        result.push_str("\n…[stdout truncated at the output limit]");
                    }
                }
                if !stderr.trim().is_empty() {
                    result.push_str(&format!("\n--- stderr ---\n{}", stderr.trim_end()));
                    if clipped(err) {
                        result.push_str("\n…[stderr truncated at the output limit]");
                    }
                }
                ToolOutput::text(result)
                    .with_title(format!("shell: {}", self.command))
                    .with_structured(json!({
                        "exit": code,
                        "truncated": clipped(out) || clipped(err),
                        "timeout": false,
                    }))
            }
            Ran::Cancelled => ToolOutput::text("Command cancelled."),
            Ran::Broken(error) => ToolOutput::text(format!("error: {error}")),
        }
    }
}

fn kill_group(pgid: Option<i32>) {
    #[cfg(unix)]
    if let Some(pgid) = pgid {
        // Safety: a plain syscall with a pid we spawned; an already-reaped group
        // just returns ESRCH, which is why the result is ignored.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pgid;
}

#[cfg(test)]
mod tests;
