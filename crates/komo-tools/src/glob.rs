//! The `glob` tool: find files by pattern.
//!
//! Half of what komo was missing to *locate* code (the other is `grep`). Before
//! this, finding a file meant asking `shell` to run `find`/`ls` — a different
//! output shape every time, a shell approval for a read-only question, and no
//! `.gitignore` awareness, so half the hits were in `target/`.

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

/// Paths returned by one call unless `limit` says fewer.
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1_000;

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    /// Subdirectory to search; defaults to the workspace root.
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

pub struct GlobTool {
    workspace: Arc<Workspace>,
}

impl GlobTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files by glob pattern, newest first. Honors .gitignore, so build \
         output and dependencies stay out of the results."
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
                    "description": "Glob matched against each file's path, e.g. `**/*.rs`."
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search; relative to the workspace root, or an absolute local path. Defaults to the root."
                },
                "limit": {
                    "type": "integer",
                    "description": format!("Maximum paths to return (default {DEFAULT_LIMIT}, maximum {MAX_LIMIT}).")
                }
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: GlobArgs = parse_args(&input)?;
        let root =
            fs_common::resolve_readable(&self.workspace, ctx, args.path.as_deref().unwrap_or("."))?;

        // The search root is the read being requested: one `ActionRef::File`
        // check, so a `file`/`access = "read"` deny rule fences off a directory
        // before the walk touches it.
        if let Some(refusal) = fs_common::allow_read(ctx, &root).await {
            return Ok(ToolOutput::text(refusal));
        }

        let matcher = search::compile_glob(&args.pattern).map_err(ToolError::InvalidInput)?;
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

        let walk_root = root.clone();
        let found = tokio::task::spawn_blocking(move || {
            search::candidates(&walk_root, |p| matcher.is_match(p), limit)
        })
        .await
        .map_err(|e| ToolError::Failed(anyhow::anyhow!("glob walk failed: {e}")))?;

        // A denied path must not even be named: a listing is itself a read.
        let mut paths: Vec<PathBuf> = Vec::with_capacity(found.items.len());
        for candidate in found.items {
            if fs_common::allow_read(ctx, &candidate.path).await.is_none() {
                paths.push(candidate.path);
            }
        }

        if paths.is_empty() {
            return Ok(ToolOutput::text(format!(
                "No files match `{}` under {}.",
                args.pattern,
                search::display_path(&root, &root)
            )));
        }

        let mut out = paths
            .iter()
            .map(|p| search::display_path(&root, p))
            .collect::<Vec<_>>()
            .join("\n");
        if found.clipped {
            out.push_str(&format!(
                "\n…stopped at {limit} results. Narrow the pattern or raise `limit` to see more."
            ));
        }

        Ok(ToolOutput::text(out)
            .with_title(format!("glob {} ({} files)", args.pattern, paths.len()))
            .with_structured(json!({ "count": paths.len(), "clipped": found.clipped })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::detached_ctx;

    /// A small tree with a gitignored directory, inside a workspace.
    fn tool_in(tag: &str) -> (GlobTool, PathBuf) {
        let dir = std::env::temp_dir().join(format!("komo_globtool_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("build")).unwrap();
        std::fs::write(dir.join(".gitignore"), "build/\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "// lib\n").unwrap();
        std::fs::write(dir.join("README.md"), "# hi\n").unwrap();
        std::fs::write(dir.join("build/gen.rs"), "// generated\n").unwrap();
        (
            GlobTool::new(Arc::new(Workspace::new(vec![dir.clone()]))),
            dir,
        )
    }

    #[tokio::test]
    async fn finds_matching_files_as_relative_paths() {
        let (tool, _dir) = tool_in("basic");
        let out = tool
            .call(json!({ "pattern": "**/*.rs" }), &detached_ctx("cli:t"))
            .await
            .unwrap();
        assert!(out.text.contains("src/main.rs"), "{}", out.text);
        assert!(out.text.contains("src/lib.rs"), "{}", out.text);
        assert!(!out.text.contains("README.md"), "{}", out.text);
        // Gitignored output stays out — the whole point of not using `find`.
        assert!(!out.text.contains("gen.rs"), "{}", out.text);
    }

    #[tokio::test]
    async fn no_match_says_so_plainly() {
        let (tool, _dir) = tool_in("nomatch");
        let out = tool
            .call(json!({ "pattern": "**/*.py" }), &detached_ctx("cli:t"))
            .await
            .unwrap();
        assert!(out.text.contains("No files match"), "{}", out.text);
    }

    #[tokio::test]
    async fn path_narrows_the_search() {
        let (tool, _dir) = tool_in("narrow");
        let out = tool
            .call(
                json!({ "pattern": "*.rs", "path": "src" }),
                &detached_ctx("cli:t"),
            )
            .await
            .unwrap();
        // Relative to the *search* root, so the `src/` prefix is gone.
        assert!(out.text.contains("main.rs"), "{}", out.text);
        assert!(!out.text.contains("src/main.rs"), "{}", out.text);
    }

    #[tokio::test]
    async fn limit_clips_and_says_it_clipped() {
        let (tool, _dir) = tool_in("limit");
        let out = tool
            .call(
                json!({ "pattern": "**/*.rs", "limit": 1 }),
                &detached_ctx("cli:t"),
            )
            .await
            .unwrap();
        assert!(out.text.contains("stopped at 1 results"), "{}", out.text);
        assert_eq!(out.structured["clipped"], true);
    }

    #[tokio::test]
    async fn a_bad_pattern_is_invalid_input() {
        let (tool, _dir) = tool_in("badpattern");
        let err = tool
            .call(json!({ "pattern": "a{b" }), &detached_ctx("cli:t"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(err.to_string().contains("invalid glob"));
    }

    #[tokio::test]
    async fn searching_outside_the_workspace_is_denied() {
        let (tool, _dir) = tool_in("escape");
        let err = tool
            .call(
                json!({ "pattern": "*", "path": "/etc" }),
                &detached_ctx("cli:t"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)));
    }

    #[tokio::test]
    async fn unrestricted_workspace_can_search_an_absolute_external_directory() {
        let (_tool, workspace_dir) = tool_in("unrestricted_workspace");
        let external = std::env::temp_dir().join("komo_globtool_unrestricted_external");
        let _ = std::fs::remove_dir_all(&external);
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(external.join("visible.txt"), "visible\n").unwrap();
        let tool = GlobTool::new(Arc::new(
            Workspace::new(vec![workspace_dir]).with_unrestricted_reads(),
        ));

        let out = tool
            .call(
                json!({ "pattern": "*.txt", "path": external.display().to_string() }),
                &detached_ctx("cli:t"),
            )
            .await
            .unwrap();

        assert!(out.text.contains("visible.txt"), "{}", out.text);
        let _ = std::fs::remove_dir_all(&external);
    }

    #[test]
    fn the_model_facing_text_stays_short() {
        crate::test_support::assert_model_text_budget(&GlobTool::new(Arc::new(Workspace::new(
            vec![],
        ))));
    }
}
