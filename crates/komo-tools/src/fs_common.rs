//! Shared plumbing for the filesystem tools (`read`, `write`, and — next —
//! `edit` / `apply_patch`).
//!
//! Three things every one of them needs, in the same order every time:
//! resolve the model's path against the workspace, ask the approver with the
//! right [`ActionRef`] (so `[policy]` rules keep matching on category/access),
//! and turn a refusal into text the model can act on.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use komo_core::domain::{
    approval::{ActionRef, ApprovalRequest, Decision},
    context::ToolContext,
    tool::ToolError,
    workspace::Workspace,
};

/// Resolve a model-supplied path inside `workspace`. Relative paths anchor to
/// the workspace root; anything that lands outside it is refused as
/// [`ToolError::Denied`] — the workspace whitelist is a floor, not a prompt (no
/// approval unlocks it, matching `shell`'s hardline patterns).
///
/// This is the **mutating** resolver. A read goes through
/// [`resolve_readable`], which also admits komo's read-only managed roots.
pub fn resolve(
    workspace: &Arc<Workspace>,
    ctx: &ToolContext,
    path: &str,
) -> Result<PathBuf, ToolError> {
    let effective = effective(workspace, ctx);
    effective.resolve_contained(Path::new(path)).ok_or_else(|| {
        ToolError::Denied(format!(
            "path `{path}` is outside the workspace and was blocked. \
                 Only paths under {} are available.",
            roots_note(effective.roots())
        ))
    })
}

/// [`resolve`] for a read: the workspace plus its read-only roots, or any local
/// path when unrestricted reads are enabled. A preview hands the model a managed
/// tool-output path, so `read`, `grep`, and `glob` have to be able to open it;
/// nothing else can, because every mutating tool resolves through [`resolve`].
pub fn resolve_readable(
    workspace: &Arc<Workspace>,
    ctx: &ToolContext,
    path: &str,
) -> Result<PathBuf, ToolError> {
    let effective = effective(workspace, ctx);
    effective.resolve_readable(Path::new(path)).ok_or_else(|| {
        ToolError::Denied(format!(
            "path `{path}` is outside the workspace and was blocked. \
                 Only paths under {} are available.",
            roots_note(
                &effective
                    .roots()
                    .iter()
                    .chain(effective.readonly_roots())
                    .cloned()
                    .collect::<Vec<_>>()
            )
        ))
    })
}

/// The workspace this turn actually resolves against: the roots the turn was
/// given, else the wired default. `roots[0]` is what a relative path anchors to,
/// and every root is writable — that is what `/workspace add` widens. Turn-given
/// roots still carry komo's own managed roots — the read-only tool-output store
/// and the writable artifacts directory are komo's, not the workspace's, so
/// moving where a turn works must not take them away.
///
/// `shell` resolves its `workdir` through this too, which is what lets a turn run
/// a command inside its artifacts directory.
pub(crate) fn effective<'a>(
    workspace: &'a Arc<Workspace>,
    ctx: &ToolContext,
) -> std::borrow::Cow<'a, Workspace> {
    let roots = &ctx.session.workspace_roots;
    if roots.is_empty() {
        return std::borrow::Cow::Borrowed(workspace.as_ref());
    }
    let derived = Workspace::new(roots.clone()).with_readonly(workspace.readonly_roots().to_vec());
    let derived = match workspace.artifacts_root() {
        Some(artifacts) => derived.with_artifacts(artifacts.to_path_buf()),
        None => derived,
    };
    let derived = match workspace.plugins_root() {
        Some(plugins) => derived.with_plugins(plugins.to_path_buf()),
        None => derived,
    };
    let derived = if workspace.has_unrestricted_reads() {
        derived.with_unrestricted_reads()
    } else {
        derived
    };
    std::borrow::Cow::Owned(derived)
}

fn roots_note(roots: &[PathBuf]) -> String {
    roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Consult the approver for a **read**. Reads are `Risk::Safe`, so an
/// interactive approver never prompts — but a `category = "file", access =
/// "read"` deny rule still blackholes the path (the exfiltration guard). Returns
/// the refusal text when blocked.
pub async fn allow_read(ctx: &ToolContext, path: &Path) -> Option<String> {
    let request =
        ApprovalRequest::safe(format!("read {}", path.display())).with_action(ActionRef::File {
            path: path.to_path_buf(),
            write: false,
        });
    let decision = ctx.decide(&request).await;
    if decision.is_allowed() {
        return None;
    }
    Some(match decision.feedback() {
        Some(reason) => format!(
            "Read of {} blocked: {reason}. Nothing was read.",
            path.display()
        ),
        None => format!(
            "Read of {} blocked by the permission policy; nothing was read.",
            path.display()
        ),
    })
}

/// Consult the approver for a **write** (`Risk::Normal` — it prompts).
/// `summary` describes the mutation for the human. Returns the refusal text,
/// carrying the user's reason when they gave one, when denied.
///
/// A write into the plugin directory is [`Risk::Dangerous`] instead — see
/// [`write_request`].
pub async fn allow_write(
    workspace: &Arc<Workspace>,
    ctx: &ToolContext,
    path: &Path,
    summary: String,
) -> Option<String> {
    let request = write_request(workspace, ctx, path, summary);
    let decision = ctx.decide(&request).await;
    if decision.is_allowed() {
        return None;
    }
    Some(match decision.feedback() {
        Some(reason) => format!(
            "Rejected by the user; {} was not changed. They said: {reason}",
            path.display()
        ),
        None => format!("Rejected by user; {} was not changed.", path.display()),
    })
}

/// The approval a write asks for, and the one place a path's *risk* is decided.
///
/// Ordinary writes are `Risk::Normal`: the operator answers, and may widen the
/// answer to the session or save it, keyed on `file:write`.
///
/// A write into the plugin directory is `Risk::Dangerous` and carries **no
/// scope key**, so it can be neither auto-allowed by an earlier `file:write`
/// grant nor widened by this answer — `Risk::Dangerous` already narrows
/// `/approve session|always` to a single call, and without a key there is
/// nothing for a session cache to hit. The reason is what the file *becomes*:
/// python saved there is loaded into the plugin host and runs unsandboxed on
/// the host, on every later turn, unattended routines included. That is a
/// standing grant of arbitrary code execution, so a human authorizes each one.
fn write_request(
    workspace: &Arc<Workspace>,
    ctx: &ToolContext,
    path: &Path,
    summary: String,
) -> ApprovalRequest {
    let action = ActionRef::File {
        path: path.to_path_buf(),
        write: true,
    };
    if effective(workspace, ctx).is_plugin_path(path) {
        return ApprovalRequest::dangerous(
            summary,
            "This is komo's plugin directory: python saved here is loaded into the \
             plugin host and runs unsandboxed on this machine, on every later turn \
             — scheduled routines included.",
        )
        .with_action(action);
    }
    ApprovalRequest::normal(summary)
        .with_scope_key("file:write")
        .with_action(action)
}

/// Approve a mutation that spans **several** files with a single prompt.
///
/// `summary` should name every target, because that is the one thing the human
/// sees. The subtlety is the second pass: after the human grants the batch, each
/// remaining path is still evaluated at `Risk::Safe` — never prompting, but
/// keeping `ActionRef::File{write:true}`, so a `[policy]` deny rule covering any
/// one of them still blocks it. Without that pass, approving a patch would be
/// approving paths the policy fences off; with a prompt per path, a five-file
/// patch would ask five times.
///
/// Returns the refusal text naming the path that was blocked.
///
/// A batch touching the plugin directory takes that risk for the whole prompt:
/// the human is approving the batch, and one plugin file in it is the thing
/// they have to be told about.
pub async fn allow_write_batch(
    workspace: &Arc<Workspace>,
    ctx: &ToolContext,
    paths: &[PathBuf],
    summary: String,
) -> Option<String> {
    let first = paths.first()?;
    let riskiest = paths
        .iter()
        .find(|path| effective(workspace, ctx).is_plugin_path(path))
        .unwrap_or(first);
    if let Some(refusal) = allow_write(workspace, ctx, riskiest, summary).await {
        return Some(refusal);
    }
    for path in paths.iter().filter(|path| *path != riskiest) {
        let request = ApprovalRequest::safe(format!("write {}", path.display())).with_action(
            ActionRef::File {
                path: path.clone(),
                write: true,
            },
        );
        if let Decision::Deny { feedback } = ctx.decide(&request).await {
            return Some(match feedback {
                Some(reason) => format!(
                    "Blocked before anything was written: {} is not writable ({reason}).",
                    path.display()
                ),
                None => format!(
                    "Blocked before anything was written: {} is not writable \
                     under the permission policy.",
                    path.display()
                ),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::detached_ctx;

    fn ws(root: &str) -> Arc<Workspace> {
        Arc::new(Workspace::new(vec![PathBuf::from(root)]))
    }

    #[test]
    fn relative_paths_anchor_to_the_workspace_root() {
        let resolved = resolve(&ws("/home/u/p"), &detached_ctx("test"), "src/main.rs").unwrap();
        assert_eq!(resolved, PathBuf::from("/home/u/p/src/main.rs"));
    }

    #[test]
    fn escapes_are_denied_not_merely_reported() {
        let err = resolve(&ws("/home/u/p"), &detached_ctx("test"), "../secret").unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)));
        // The message names the allowed root so the model can retry sensibly.
        assert!(err.to_string().contains("/home/u/p"));
    }

    fn ws_with_plugins() -> Arc<Workspace> {
        Arc::new(
            Workspace::new(vec![PathBuf::from("/home/u/p")])
                .with_plugins(PathBuf::from("/home/u/.komo/plugins")),
        )
    }

    /// An ordinary write prompts at `Normal` and may be widened — that is what
    /// the `file:write` scope key is for.
    #[test]
    fn an_ordinary_write_is_normal_and_widenable() {
        let request = write_request(
            &ws_with_plugins(),
            &detached_ctx("test"),
            Path::new("/home/u/p/src/main.rs"),
            "write src/main.rs".into(),
        );
        assert_eq!(request.risk, komo_core::domain::approval::Risk::Normal);
        assert_eq!(request.scope_key.as_deref(), Some("file:write"));
    }

    /// A write into the plugin directory installs code komo will run itself, on
    /// every later turn including an unattended routine's — so it is
    /// `Dangerous` (which no saved or job grant can allow) and carries no scope
    /// key, so no earlier `file:write` session grant can allow it either and
    /// this answer cannot widen into one.
    #[test]
    fn a_plugin_write_is_dangerous_and_never_widens() {
        let request = write_request(
            &ws_with_plugins(),
            &detached_ctx("test"),
            Path::new("/home/u/.komo/plugins/notes.py"),
            "write notes.py".into(),
        );
        assert_eq!(request.risk, komo_core::domain::approval::Risk::Dangerous);
        assert_eq!(
            request.scope_key, None,
            "a scope key is what a session grant hits; a plugin write must have none"
        );
        // The human is told what the file becomes, not just where it goes.
        let detail = request.detail.expect("a dangerous request explains itself");
        assert!(detail.contains("unsandboxed"), "{detail}");
    }

    /// The workspace a *session* picked keeps komo's plugin root, so a turn
    /// working elsewhere does not quietly downgrade a plugin write to `Normal`.
    #[test]
    fn a_session_workspace_still_classifies_plugin_writes() {
        let mut session = komo_core::domain::context::SessionContext::detached("test");
        session.workspace_roots = vec![PathBuf::from("/home/u/elsewhere")];
        let ctx = komo_core::domain::context::ToolContext::new(
            session,
            None,
            std::sync::Arc::new(crate::test_support::SafeOnly),
        );
        let request = write_request(
            &ws_with_plugins(),
            &ctx,
            Path::new("/home/u/.komo/plugins/notes.py"),
            "write notes.py".into(),
        );
        assert_eq!(request.risk, komo_core::domain::approval::Risk::Dangerous);
    }

    #[test]
    fn selected_workspace_overrides_the_process_default() {
        let ctx = ctx_rooted_at(vec![PathBuf::from("/home/u/selected")]);
        let resolved = resolve(&ws("/home/u/default"), &ctx, "src/main.rs").unwrap();
        assert_eq!(resolved, PathBuf::from("/home/u/selected/src/main.rs"));
    }

    /// A task that ran `/workspace add` works across both directories, and the
    /// **first** one stays the anchor — a relative path must not start meaning
    /// something else the moment a second project is admitted.
    #[test]
    fn a_second_root_is_writable_and_the_first_stays_the_anchor() {
        let ctx = ctx_rooted_at(vec![
            PathBuf::from("/home/u/primary"),
            PathBuf::from("/home/u/library"),
        ]);
        let workspace = ws("/home/u/default");
        assert_eq!(
            resolve(&workspace, &ctx, "src/main.rs").unwrap(),
            PathBuf::from("/home/u/primary/src/main.rs")
        );
        assert_eq!(
            resolve(&workspace, &ctx, "/home/u/library/notes.md").unwrap(),
            PathBuf::from("/home/u/library/notes.md")
        );
        // Everything else is still outside, added root or not.
        assert!(resolve(&workspace, &ctx, "/home/u/default/x").is_err());
    }

    fn ctx_rooted_at(roots: Vec<PathBuf>) -> komo_core::domain::context::ToolContext {
        let mut session = komo_core::domain::context::SessionContext::detached("test");
        session.workspace_roots = roots;
        komo_core::domain::context::ToolContext::new(
            session,
            None,
            std::sync::Arc::new(crate::test_support::SafeOnly),
        )
    }
}
