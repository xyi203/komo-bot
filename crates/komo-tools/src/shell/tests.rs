use super::*;
use komo_core::domain::approval::{Approver, Decision, Risk};
use komo_core::domain::context::{SessionContext, ToolContext};
use std::sync::Mutex;

fn ctx_with(approver: Arc<dyn Approver>) -> ToolContext {
    ToolContext::new(SessionContext::detached("cli:test"), None, approver)
}

struct AlwaysApprove;
#[async_trait::async_trait]
impl Approver for AlwaysApprove {
    async fn decide(&self, _request: &ApprovalRequest) -> Decision {
        Decision::Allow
    }
}

struct AlwaysReject;
#[async_trait::async_trait]
impl Approver for AlwaysReject {
    async fn decide(&self, _request: &ApprovalRequest) -> Decision {
        Decision::deny()
    }
}

/// Refuses, but explains why (`/deny <理由>` / the TUI's reason prompt).
struct RejectWithReason(&'static str);
#[async_trait::async_trait]
impl Approver for RejectWithReason {
    async fn decide(&self, _request: &ApprovalRequest) -> Decision {
        Decision::deny_because(self.0)
    }
}

/// Records the risk level of the last request it saw.
struct Recording {
    risk: Mutex<Option<Risk>>,
    approve: bool,
}

#[async_trait::async_trait]
impl Approver for Recording {
    async fn decide(&self, request: &ApprovalRequest) -> Decision {
        *self.risk.lock().unwrap() = Some(request.risk);
        self.approve.into()
    }
}

fn workspace() -> Arc<Workspace> {
    Arc::new(Workspace::new(vec![std::env::temp_dir()]))
}

/// A fresh directory under the workspace root, for the workdir/orphan tests.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("komo_shell_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn exit_status_is_also_reported_structurally() {
    let tool = ShellTool::new(workspace());
    let out = tool
        .call(
            json!({ "command": "exit 3" }),
            &ctx_with(Arc::new(AlwaysApprove)),
        )
        .await
        .unwrap();
    assert_eq!(out.structured["exit"], 3);
    assert_eq!(out.structured["timeout"], false);
    assert_eq!(out.structured["truncated"], false);
}

#[tokio::test]
async fn a_slow_command_times_out_and_says_how_to_retry() {
    let tool = ShellTool::new(workspace());
    let out = tool
        .call(
            json!({ "command": "sleep 30", "timeout": 200 }),
            &ctx_with(Arc::new(AlwaysApprove)),
        )
        .await
        .unwrap();
    assert_eq!(out.structured["timeout"], true);
    assert!(out.text.contains("timed out"), "{}", out.text);
    // The model needs to know the knob exists, or it will just retry as-is.
    assert!(out.text.contains("timeout"), "{}", out.text);
}

/// The bug `process_group` + `killpg` fixes: killing `sh` alone leaves the
/// processes it started running. Here a backgrounded child would create a
/// marker file one second in — it must never get the chance.
/// A timeout must end the whole tree, not just `sh` — a killed shell that
/// leaves a build (or a `sleep`) running is the failure this tool's process
/// group exists to prevent.
///
/// Deliberately not "wait a while and check a file did not appear": that
/// races the orphan's own clock against a loaded machine's scheduler, and
/// loses on a busy CI box. The orphan announces its pid instead, and the
/// test polls until that pid is gone — an answer about the process itself,
/// not about who won a stopwatch.
#[tokio::test]
async fn a_timeout_kills_processes_the_command_started() {
    let dir = scratch("orphan");
    let pid_file = dir.join("orphan.pid");
    let tool = ShellTool::new(workspace());
    // The orphan outlives the tool's budget by two orders of magnitude, so
    // "still running" can only mean it was never killed.
    // `$!`, not `$$`: in POSIX sh a subshell's `$$` is still the *parent*
    // shell's pid, which `kill_on_drop` reaps anyway — the test would then
    // pass without the process group doing anything.
    let command = format!("sleep 30 & echo $! > {}; sleep 30", pid_file.display());

    let out = tool
        .call(
            json!({ "command": command, "timeout": 200 }),
            &ctx_with(Arc::new(AlwaysApprove)),
        )
        .await
        .unwrap();
    assert_eq!(out.structured["timeout"], true);

    let Some(pid) = read_pid(&pid_file).await else {
        // Killed before it could even name itself — the outcome under test,
        // reached sooner. Nothing left to check.
        return;
    };
    assert!(
        wait_until_gone(pid).await,
        "a process started by the command survived the timeout (pid {pid})"
    );
}

/// The orphan's pid once it has written one, or `None` if it never does.
async fn read_pid(path: &std::path::Path) -> Option<i32> {
    for _ in 0..40 {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse::<i32>()
        {
            return Some(pid);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    None
}

/// Poll `kill(pid, 0)` until the process is gone. `true` if it went.
async fn wait_until_gone(pid: i32) -> bool {
    for _ in 0..40 {
        // Safety: signal 0 performs the permission/existence check only and
        // sends nothing.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

/// A [`CancelSignal`] that fires after a delay — a stand-in for the user
/// hitting stop mid-command.
struct CancelAfter(std::time::Duration);
#[async_trait::async_trait]
impl komo_core::domain::cancel::CancelSignal for CancelAfter {
    fn is_cancelled(&self) -> bool {
        false
    }
    async fn cancelled(&self) {
        tokio::time::sleep(self.0).await;
    }
}

/// What 14 is actually for: cancelling a turn ends the *command*, not just
/// komo's wait for it. Same orphan-marker shape as the timeout test — the
/// backgrounded child must never get to write.
#[tokio::test]
async fn cancelling_the_turn_kills_the_command_and_its_children() {
    let dir = scratch("cancel");
    let marker = dir.join("alive.txt");
    let tool = ShellTool::new(workspace());
    // Tighter than the timeout test's timings on purpose: this test holds a
    // runtime while it sleeps, and the whole suite runs concurrently.
    let command = format!(
        "(sleep 0.5; echo alive > {}) & sleep 30",
        marker.to_string_lossy()
    );
    let session = SessionContext::detached("cli:test")
        .with_cancel(Arc::new(CancelAfter(std::time::Duration::from_millis(100))));
    let ctx = ToolContext::new(session, None, Arc::new(AlwaysApprove));

    let started = std::time::Instant::now();
    // A generous `timeout` so the command's own budget can't be what ends it.
    let err = tool
        .call(json!({ "command": command, "timeout": 60_000 }), &ctx)
        .await
        .unwrap_err();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "cancel should land promptly, took {:?}",
        started.elapsed()
    );
    // The ledger wording matches the run's own cancellation, so a step and
    // its run don't describe the same stop two different ways.
    assert_eq!(
        err.to_string(),
        komo_core::domain::cancel::CANCELLED_ERROR,
        "{err}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    assert!(
        !marker.exists(),
        "a process started by the command survived cancellation"
    );
}

/// Without a cancel signal (sweeps, cron, aux) the new `select!` arm must be
/// inert — a turn nobody can interrupt behaves exactly as before.
#[tokio::test]
async fn a_turn_with_no_cancel_signal_runs_normally() {
    let tool = ShellTool::new(workspace());
    let out = tool
        .call(
            json!({ "command": "echo hi" }),
            &ctx_with(Arc::new(AlwaysApprove)),
        )
        .await
        .unwrap();
    assert!(out.text.contains("hi"), "{}", out.text);
}

#[tokio::test]
async fn workdir_runs_the_command_there() {
    let dir = scratch("workdir");
    std::fs::write(dir.join("marker.txt"), "x").unwrap();
    let tool = ShellTool::new(workspace());
    let out = tool
        .call(
            json!({ "command": "ls", "workdir": dir.to_string_lossy() }),
            &ctx_with(Arc::new(AlwaysApprove)),
        )
        .await
        .unwrap();
    assert!(out.text.contains("marker.txt"), "{}", out.text);
}

/// The artifacts directory is a writable root outside the workspace, so a
/// turn can run a command inside what it just produced there.
#[tokio::test]
async fn a_workdir_inside_the_artifacts_root_is_allowed() {
    let artifacts = scratch("artifacts");
    let session_dir = artifacts.join("cli-t");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(session_dir.join("report.md"), "x").unwrap();
    let tool = ShellTool::new(Arc::new(
        Workspace::new(vec![std::path::PathBuf::from("/home/user/project")])
            .with_artifacts(artifacts.clone()),
    ));
    let out = tool
        .call(
            json!({ "command": "ls", "workdir": session_dir.to_string_lossy() }),
            &ctx_with(Arc::new(AlwaysApprove)),
        )
        .await
        .unwrap();
    assert!(out.text.contains("report.md"), "{}", out.text);
}

#[tokio::test]
async fn a_workdir_outside_the_workspace_is_denied() {
    let tool = ShellTool::new(Arc::new(Workspace::new(vec![std::path::PathBuf::from(
        "/home/user/project",
    )])));
    let err = tool
        .call(
            json!({ "command": "ls", "workdir": "/etc" }),
            &ctx_with(Arc::new(AlwaysApprove)),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Denied(_)));
}

#[tokio::test]
async fn a_workdir_that_is_not_a_directory_is_invalid_input() {
    let dir = scratch("notadir");
    let file = dir.join("f.txt");
    std::fs::write(&file, "x").unwrap();
    let tool = ShellTool::new(workspace());
    let err = tool
        .call(
            json!({ "command": "ls", "workdir": file.to_string_lossy() }),
            &ctx_with(Arc::new(AlwaysApprove)),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidInput(_)));
}

/// A model asking for an hour gets the ceiling, not an error.
#[tokio::test]
async fn an_over_large_timeout_is_clamped_not_refused() {
    let tool = ShellTool::new(workspace());
    let out = tool
        .call(
            json!({ "command": "true", "timeout": 60 * 60 * 1000 }),
            &ctx_with(Arc::new(AlwaysApprove)),
        )
        .await
        .unwrap();
    assert_eq!(out.structured["exit"], 0);
}

#[tokio::test]
async fn approved_command_runs() {
    let tool = ShellTool::new(workspace());
    let out = tool
        .call(
            json!({ "command": "echo hello" }),
            &ctx_with(Arc::new(AlwaysApprove)),
        )
        .await
        .unwrap();
    assert!(out.text.contains("hello"));
    assert!(out.text.contains("exit status: 0"));
}

#[tokio::test]
async fn rejected_command_does_not_run() {
    let tool = ShellTool::new(workspace());
    let out = tool
        .call(
            json!({ "command": "rm -r should_not_appear" }),
            &ctx_with(Arc::new(AlwaysReject)),
        )
        .await
        .unwrap();
    assert!(out.text.contains("rejected"));
    assert!(!out.text.contains("--- stdout ---"));
}

#[tokio::test]
async fn a_denial_reason_is_relayed_to_the_model() {
    let tool = ShellTool::new(workspace());
    let out = tool
        .call(
            json!({ "command": "rm -f /tmp/komo_shell_reason" }),
            &ctx_with(Arc::new(RejectWithReason("用 trash 代替 rm"))),
        )
        .await
        .unwrap();
    assert!(out.text.contains("用 trash 代替 rm"), "got: {}", out.text);
    // And it must be told not to just try again.
    assert!(out.text.contains("instead of retrying"));
    assert!(!out.text.contains("--- stdout ---"), "nothing ran");
}

#[tokio::test]
async fn hardline_command_is_refused_without_consulting_approver() {
    let rec = Arc::new(Recording {
        risk: Mutex::new(None),
        approve: true,
    });
    let tool = ShellTool::new(workspace());
    let out = tool
        .call(
            json!({ "command": "sudo rm -rf / --no-preserve-root" }),
            &ctx_with(rec.clone()),
        )
        .await
        .unwrap();
    assert!(out.text.contains("refused"));
    // The approver was never asked: hardline sits above the approval gate.
    assert_eq!(*rec.risk.lock().unwrap(), None);
}

#[tokio::test]
async fn dangerous_commands_are_flagged() {
    for cmd in ["rm -rf foo", "git push origin main"] {
        let rec = Arc::new(Recording {
            risk: Mutex::new(None),
            approve: false,
        });
        let tool = ShellTool::new(workspace());
        let _ = tool
            .call(json!({ "command": cmd }), &ctx_with(rec.clone()))
            .await;
        assert_eq!(
            *rec.risk.lock().unwrap(),
            Some(Risk::Dangerous),
            "cmd: {cmd}"
        );
    }
}

#[test]
fn dangerous_pattern_matches_at_command_boundary() {
    // Real dangerous commands still match.
    assert_eq!(dangerous_pattern("rm -rf foo"), Some("rm "));
    assert_eq!(dangerous_pattern("git push origin main"), Some("git push"));
    // ...including when chained after a shell separator.
    assert_eq!(dangerous_pattern("cd /tmp && rm -rf x"), Some("rm "));

    // `kill -TERM 1` is dangerous because of `kill `, NOT a stray `rm ` buried
    // in "-te*rm* 1" (the bug that mislabeled the prompt as `rm`).
    assert_eq!(dangerous_pattern("kill -TERM 1"), Some("kill "));

    // Innocuous commands that merely contain a pattern as a substring inside
    // a word must not be flagged.
    assert_eq!(dangerous_pattern("terraform apply"), None);
    assert_eq!(dangerous_pattern("echo perform task"), None);
}

#[test]
fn redact_secrets_scrubs_common_shapes() {
    let cmd = "curl -H 'Authorization: Bearer sk-abc123def456ghi789' https://api.example.com";
    let r = redact_secrets(cmd);
    assert!(!r.contains("sk-abc123def456ghi789"));
    assert!(r.contains("Bearer"));

    let kv = redact_secrets("deploy --env api_key=AKIA1234567890SECRET token=zzz");
    assert!(!kv.contains("AKIA1234567890SECRET"));
    assert!(kv.contains("api_key=***"));

    let flag = redact_secrets("login --password hunter2longenoughxx");
    assert!(!flag.contains("hunter2longenoughxx"));
    assert!(flag.contains("--password ***"));

    let entropy = redact_secrets("echo ABCD1234efgh5678ijkl9012mnop");
    assert!(entropy.contains("***"));

    // Ordinary commands pass through untouched.
    assert_eq!(redact_secrets("ls -la /tmp"), "ls -la /tmp");
}

#[test]
fn redact_args_scrubs_command_value() {
    let tool = ShellTool::new(workspace());
    let args = json!({ "command": "x token=supersecretvalue123456" }).to_string();
    let redacted = tool.redact_args(&args);
    assert!(!redacted.contains("supersecretvalue123456"));
}

#[tokio::test]
async fn output_is_bounded_at_the_stream_limit() {
    // A command that emits more than the stream cap must be truncated, not
    // buffered whole into memory.
    let tool = ShellTool::new(workspace());
    let bytes = MAX_STREAM_BYTES + 50_000;
    let out = tool
        .call(
            json!({ "command": format!("yes a | head -c {bytes}") }),
            &ctx_with(Arc::new(AlwaysApprove)),
        )
        .await
        .unwrap();
    assert!(
        out.text.contains("stdout truncated"),
        "expected a truncation marker, got {} bytes",
        out.text.len()
    );
}

#[tokio::test]
async fn command_reading_stdin_sees_eof_instead_of_hanging() {
    // stdin is wired to /dev/null, so a command that reads stdin gets EOF
    // and exits promptly rather than blocking forever waiting for input.
    let tool = ShellTool::new(workspace());
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tool.call(
            json!({ "command": "cat" }),
            &ctx_with(Arc::new(AlwaysApprove)),
        ),
    )
    .await
    .expect("cat must not hang on stdin")
    .unwrap();
    assert!(out.text.contains("exit status: 0"));
}

#[tokio::test]
async fn safe_commands_are_safe_risk() {
    let rec = Arc::new(Recording {
        risk: Mutex::new(None),
        approve: true,
    });
    let tool = ShellTool::new(workspace());
    let _ = tool
        .call(json!({ "command": "echo hi" }), &ctx_with(rec.clone()))
        .await;
    assert_eq!(*rec.risk.lock().unwrap(), Some(Risk::Safe));
}
