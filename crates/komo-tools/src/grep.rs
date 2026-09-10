//! The `grep` tool: locate code, by contents or by filename.
//!
//! Output copies opencode v2's shape — a count line, then `path:` blocks of
//! `Line N: text` — because that is what models have been trained on, and
//! because it is compact enough that a wide search still fits in a turn.
//!
//! ## Why finding files is the same tool
//!
//! Searching contents and finding filenames were two tools (`grep` and `glob`)
//! that already shared one walk: [`search::candidates`] collects the paths, the
//! policy filters them, and only then is anything opened. `glob` stopped after
//! the walk; `grep` went on to read. So the second tool was a schema block —
//! re-sent every round of every turn — for a code path that already existed.
//!
//! Omitting `pattern` is what asks for the walk alone. `include` is then
//! required: without either, the call is "list the entire tree", which is a
//! footgun rather than a question. The newest-first order comes from the shared
//! walk, so a filename search still puts what someone just touched on top.
//!
//! Permission order matters here: the walk collects candidate paths first, the
//! policy filters them, and only the survivors are opened. A `file` deny rule
//! therefore prevents the content from being read at all, not merely from being
//! shown.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::fs_common;
use komo_core::domain::{
    context::ToolContext,
    tool::{Tool, ToolError, ToolOutput, parse_args},
    workspace::Workspace,
};
use komo_services::search;

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1_000;
/// Per-line preview cap, so one minified line can't fill the whole result.
const MAX_LINE_CHARS: usize = 400;

#[derive(Deserialize)]
struct GrepArgs {
    /// Absent means "find files, don't read them" — see the module docs.
    #[serde(default)]
    pattern: Option<String>,
    /// Directory or single file to search; defaults to the workspace root.
    #[serde(default)]
    path: Option<String>,
    /// Glob restricting which files are searched, e.g. `*.{ts,tsx}`.
    #[serde(default)]
    include: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

pub struct GrepTool {
    workspace: Arc<Workspace>,
}

impl GrepTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }

    /// The walk without the read: paths matching `matcher`, newest first.
    ///
    /// A denied path is not even named — a listing is itself a read, and the
    /// same filter runs here as on the content path.
    async fn list_files(
        &self,
        ctx: &ToolContext,
        root: &std::path::Path,
        matcher: search::GlobMatcher,
        limit: usize,
    ) -> Result<ToolOutput, ToolError> {
        let walk_root = root.to_path_buf();
        let found = tokio::task::spawn_blocking(move || {
            search::candidates(&walk_root, |p| matcher.is_match(p), limit)
        })
        .await
        .map_err(|e| ToolError::Failed(anyhow::anyhow!("grep walk failed: {e}")))?;

        let mut paths: Vec<std::path::PathBuf> = Vec::with_capacity(found.items.len());
        for candidate in found.items {
            if fs_common::allow_read(ctx, &candidate.path).await.is_none() {
                paths.push(candidate.path);
            }
        }

        if paths.is_empty() {
            return Ok(ToolOutput::text(format!(
                "No files match under {}.",
                search::display_path(root, root)
            ))
            .with_structured(json!({ "count": 0, "clipped": false })));
        }

        let mut out = paths
            .iter()
            .map(|p| search::display_path(root, p))
            .collect::<Vec<_>>()
            .join("\n");
        if found.clipped {
            out.push_str(&format!(
                "\n…stopped at {limit} results. Narrow `include` or raise `limit`."
            ));
        }
        let count = paths.len();
        Ok(ToolOutput::text(out)
            .with_title(format!("{count} file(s)"))
            .with_structured(json!({ "count": count, "clipped": found.clipped })))
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Find code. With `pattern`, searches file contents by regex and returns \
         matching lines; with `include` alone, lists the files whose paths match, \
         newest first. Honors .gitignore and skips binaries."
    }

    fn idempotent(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex matched against file contents. Omit to list matching filenames instead of searching them."
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search; relative to the workspace root, or an absolute local path. Defaults to the root."
                },
                "include": {
                    "type": "string",
                    "description": "Glob on the file path, e.g. `**/*.rs`. Narrows a content search; required when `pattern` is omitted."
                },
                "limit": {
                    "type": "integer",
                    "description": format!("Maximum matches to return (default {DEFAULT_LIMIT}, maximum {MAX_LIMIT}).")
                }
            },
            "required": []
        })
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: GrepArgs = parse_args(&input)?;
        let target =
            fs_common::resolve_readable(&self.workspace, ctx, args.path.as_deref().unwrap_or("."))?;

        if let Some(refusal) = fs_common::allow_read(ctx, &target).await {
            return Ok(ToolOutput::text(refusal));
        }

        let include = match &args.include {
            Some(glob) => Some(search::compile_glob(glob).map_err(ToolError::InvalidInput)?),
            None => None,
        };
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

        // No content pattern: the walk *is* the answer. Bounded by `limit`
        // rather than by `MAX_CANDIDATES`, because here every candidate is a
        // result rather than a file to open.
        let Some(pattern) = args.pattern.as_deref().map(str::to_string) else {
            let Some(matcher) = include else {
                return Err(ToolError::InvalidInput(
                    "give a `pattern` to search contents, or an `include` glob to list \
                     filenames — without either this would list the whole tree"
                        .to_string(),
                ));
            };
            return self.list_files(ctx, &target, matcher, limit).await;
        };
        let matcher = search::compile_regex(&pattern).map_err(ToolError::InvalidInput)?;

        // One file or a whole tree: a single file skips the walk entirely.
        let is_file = tokio::fs::metadata(&target)
            .await
            .map(|m| m.is_file())
            .unwrap_or(false);
        let root = if is_file {
            target.parent().unwrap_or(&target).to_path_buf()
        } else {
            target.clone()
        };

        let (candidates, walk_clipped) = if is_file {
            (vec![target.clone()], false)
        } else {
            let walk_root = root.clone();
            let found = tokio::task::spawn_blocking(move || {
                search::candidates(
                    &walk_root,
                    |p| include.as_ref().is_none_or(|g| g.is_match(p)),
                    search::MAX_CANDIDATES,
                )
            })
            .await
            .map_err(|e| ToolError::Failed(anyhow::anyhow!("grep walk failed: {e}")))?;
            (
                found.items.into_iter().map(|c| c.path).collect(),
                found.clipped,
            )
        };

        // Filter *before* reading: a denied file's contents are never opened.
        let mut allowed: Vec<PathBuf> = Vec::with_capacity(candidates.len());
        for path in candidates {
            if fs_common::allow_read(ctx, &path).await.is_none() {
                allowed.push(path);
            }
        }

        let searched = allowed.len();
        let found =
            tokio::task::spawn_blocking(move || search::search_files(&allowed, &matcher, limit))
                .await
                .map_err(|e| ToolError::Failed(anyhow::anyhow!("grep failed: {e}")))?;

        if found.items.is_empty() {
            return Ok(ToolOutput::text(format!(
                "No matches for `{pattern}` in {searched} file(s).",
            ))
            .with_structured(json!({ "matches": 0, "files_searched": searched })));
        }

        // v2's shape: a count, then one block per file.
        let mut lines = vec![format!("Found {} matches", found.items.len())];
        let mut current = String::new();
        for m in &found.items {
            let shown = search::display_path(&root, &m.path);
            if shown != current {
                if !current.is_empty() {
                    lines.push(String::new());
                }
                current = shown.clone();
                lines.push(format!("{shown}:"));
            }
            lines.push(format!("  Line {}: {}", m.line, clip(&m.text)));
        }
        if found.clipped {
            lines.push(format!(
                "\n…stopped at {limit} matches. Narrow the pattern, or use `include`/`path`."
            ));
        }
        if walk_clipped {
            lines.push(format!(
                "(only the {} most recently modified files were searched)",
                search::MAX_CANDIDATES
            ));
        }

        let files = found
            .items
            .iter()
            .map(|m| m.path.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        Ok(ToolOutput::text(lines.join("\n"))
            .with_title(format!(
                "grep {pattern} ({} matches in {files} files)",
                found.items.len()
            ))
            .with_structured(json!({
                "matches": found.items.len(),
                "files_matched": files,
                "files_searched": searched,
                "clipped": found.clipped,
            })))
    }
}

/// Clip one preview line at a char boundary.
fn clip(text: &str) -> String {
    if text.chars().count() <= MAX_LINE_CHARS {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX_LINE_CHARS).collect();
    out.push_str(" …[clipped]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::detached_ctx;

    fn tool_in(tag: &str) -> (GrepTool, PathBuf) {
        let dir = std::env::temp_dir().join(format!("komo_greptool_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("build")).unwrap();
        std::fs::write(dir.join(".gitignore"), "build/\n").unwrap();
        std::fs::write(
            dir.join("src/main.rs"),
            "fn main() {\n    let needle = 1;\n}\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), "// no match here\n").unwrap();
        std::fs::write(dir.join("notes.md"), "needle in prose\n").unwrap();
        std::fs::write(dir.join("build/gen.rs"), "let needle = 2;\n").unwrap();
        (
            GrepTool::new(Arc::new(Workspace::new(vec![dir.clone()]))),
            dir,
        )
    }

    /// The filename half: `include` with no `pattern` lists paths instead of
    /// reading them, and .gitignore still applies.
    #[tokio::test]
    async fn include_without_a_pattern_lists_filenames() {
        let (tool, _dir) = tool_in("files_only");
        let out = tool
            .call(json!({ "include": "**/*.rs" }), &detached_ctx("s"))
            .await
            .unwrap();
        assert!(out.text.contains("src/main.rs"), "{}", out.text);
        assert!(out.text.contains("src/lib.rs"), "{}", out.text);
        assert!(
            !out.text.contains("notes.md"),
            "the glob should exclude it: {}",
            out.text
        );
        assert!(
            !out.text.contains("build/gen.rs"),
            "gitignored: {}",
            out.text
        );
        // Paths, not contents — the walk stops before anything is opened.
        assert!(!out.text.contains("needle"), "{}", out.text);
    }

    /// Neither argument would mean "list the whole tree". That is a footgun,
    /// not a question, so it is refused with the two ways to ask one.
    #[tokio::test]
    async fn neither_pattern_nor_include_is_refused() {
        let (tool, _dir) = tool_in("no_args");
        let err = tool.call(json!({}), &detached_ctx("s")).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
        assert!(err.to_string().contains("include"));
    }

    /// A stored over-limit result is only useful if the model can search the
    /// part the preview elided — so `grep` has to reach the managed root too.
    #[tokio::test]
    async fn searches_a_managed_path_outside_the_workspace() {
        let base = std::env::temp_dir().join("komo_greptool_managed");
        let _ = std::fs::remove_dir_all(&base);
        let workspace_dir = base.join("project");
        let managed = base.join("tool-output").join("cli-t");
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::create_dir_all(&managed).unwrap();
        let stored = managed.join("run-0000.txt");
        std::fs::write(&stored, "head\nelided middle marker\ntail\n").unwrap();

        let tool = GrepTool::new(Arc::new(
            Workspace::new(vec![workspace_dir]).with_readonly(vec![base.join("tool-output")]),
        ));
        let out = tool
            .call(
                json!({ "pattern": "elided middle", "path": stored.display().to_string() }),
                &detached_ctx("cli:t"),
            )
            .await
            .unwrap();
        assert!(out.text.contains("elided middle marker"), "{}", out.text);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn reports_path_line_and_text() {
        let (tool, _dir) = tool_in("basic");
        let out = tool
            .call(json!({ "pattern": "needle" }), &detached_ctx("cli:t"))
            .await
            .unwrap();
        assert!(out.text.starts_with("Found "), "{}", out.text);
        assert!(out.text.contains("src/main.rs:"), "{}", out.text);
        // Leading indentation is preserved — it is meaningful in code.
        assert!(
            out.text.contains("Line 2:     let needle = 1;"),
            "{}",
            out.text
        );
        assert!(out.text.contains("notes.md:"), "{}", out.text);
        // Gitignored files are not searched.
        assert!(!out.text.contains("gen.rs"), "{}", out.text);
    }

    #[tokio::test]
    async fn include_limits_which_files_are_searched() {
        let (tool, _dir) = tool_in("include");
        let out = tool
            .call(
                json!({ "pattern": "needle", "include": "*.rs" }),
                &detached_ctx("cli:t"),
            )
            .await
            .unwrap();
        assert!(out.text.contains("src/main.rs"), "{}", out.text);
        assert!(!out.text.contains("notes.md"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_single_file_path_searches_only_that_file() {
        let (tool, _dir) = tool_in("onefile");
        let out = tool
            .call(
                json!({ "pattern": "needle", "path": "notes.md" }),
                &detached_ctx("cli:t"),
            )
            .await
            .unwrap();
        assert_eq!(out.structured["files_searched"], 1);
        assert!(out.text.contains("notes.md"), "{}", out.text);
    }

    #[tokio::test]
    async fn no_match_reports_how_many_files_were_searched() {
        let (tool, _dir) = tool_in("nomatch");
        let out = tool
            .call(json!({ "pattern": "zzzz" }), &detached_ctx("cli:t"))
            .await
            .unwrap();
        assert!(out.text.contains("No matches"), "{}", out.text);
        assert_eq!(out.structured["matches"], 0);
    }

    #[tokio::test]
    async fn limit_clips_and_says_it_clipped() {
        let (tool, _dir) = tool_in("limit");
        let out = tool
            .call(
                json!({ "pattern": "needle", "limit": 1 }),
                &detached_ctx("cli:t"),
            )
            .await
            .unwrap();
        assert_eq!(out.structured["matches"], 1);
        assert!(out.text.contains("stopped at 1 matches"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_bad_regex_is_invalid_input() {
        let (tool, _dir) = tool_in("badregex");
        let err = tool
            .call(json!({ "pattern": "(unclosed" }), &detached_ctx("cli:t"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(err.to_string().contains("invalid regular expression"));
    }

    /// The exfiltration guard: a `file`/read deny rule must stop grep from
    /// opening the file, not merely from printing it.
    #[tokio::test]
    async fn a_denied_path_is_never_searched() {
        struct DenySecrets;
        #[async_trait::async_trait]
        impl komo_core::domain::approval::Approver for DenySecrets {
            async fn decide(
                &self,
                r: &komo_core::domain::approval::ApprovalRequest,
            ) -> komo_core::domain::approval::Decision {
                let secret = match &r.action {
                    Some(komo_core::domain::approval::ActionRef::File { path, .. }) => {
                        path.to_string_lossy().contains("secrets")
                    }
                    _ => false,
                };
                if secret {
                    komo_core::domain::approval::Decision::deny_because("off limits")
                } else {
                    komo_core::domain::approval::Decision::Allow
                }
            }
        }

        let (tool, dir) = tool_in("denied");
        std::fs::write(dir.join("secrets.env"), "needle = hunter2\n").unwrap();
        let ctx = komo_core::domain::context::ToolContext::new(
            komo_core::domain::context::SessionContext::detached("cli:t"),
            None,
            Arc::new(DenySecrets),
        );
        let out = tool
            .call(json!({ "pattern": "needle" }), &ctx)
            .await
            .unwrap();
        assert!(out.text.contains("src/main.rs"), "{}", out.text);
        assert!(!out.text.contains("secrets"), "{}", out.text);
        assert!(!out.text.contains("hunter2"), "{}", out.text);
    }

    #[test]
    fn long_lines_are_clipped_on_a_char_boundary() {
        let clipped = clip(&"界".repeat(MAX_LINE_CHARS + 10));
        assert!(clipped.ends_with("…[clipped]"));
    }

    #[test]
    fn the_model_facing_text_stays_short() {
        crate::test_support::assert_model_text_budget(&GrepTool::new(Arc::new(Workspace::new(
            vec![],
        ))));
    }
}
