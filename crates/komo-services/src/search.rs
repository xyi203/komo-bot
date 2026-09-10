//! Filesystem search: the walk + match layer behind the `glob` and `grep` tools.
//!
//! Built on ripgrep's own components (`ignore` + `globset` + `grep-searcher`)
//! rather than shelling out to an `rg` binary — komo ships as a single binary
//! and can't assume one is installed. That also means one place decides what is
//! skipped (`.gitignore`, `.git/`, hidden files, binaries) instead of every
//! model-authored `shell` invocation deciding differently.
//!
//! Everything here is **blocking**: the walker and searcher are synchronous, so
//! the tools call these inside `spawn_blocking`. Splitting the work into
//! `candidates` (which paths) and `search_files` (what's in them) is what lets a
//! tool run the permission policy over the paths *before* any content is read.

use std::path::{Path, PathBuf};

use globset::Glob;
/// Re-exported so a caller can *name* what [`compile_glob`] hands back without
/// taking its own dependency on globset.
pub use globset::GlobMatcher;
use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::WalkBuilder;

/// Hard ceiling on how many paths one walk collects. A pathological tree (or a
/// missing `.gitignore`) shouldn't turn one tool call into a full-disk scan; the
/// caller is told when this clips so it can narrow the search instead of
/// believing it saw everything.
pub const MAX_CANDIDATES: usize = 20_000;

/// A file the walk found, with its modification time for recency ordering.
pub struct Candidate {
    pub path: PathBuf,
    /// Seconds since the epoch; `0` when the platform/filesystem won't say.
    pub modified: u64,
}

/// One matching line.
pub struct Match {
    pub path: PathBuf,
    pub line: u64,
    pub text: String,
}

/// The outcome of a bounded operation: results plus whether a limit clipped them.
pub struct Bounded<T> {
    pub items: Vec<T>,
    pub clipped: bool,
}

/// Compile a glob. `Err` carries a model-facing explanation.
pub fn compile_glob(pattern: &str) -> Result<GlobMatcher, String> {
    Glob::new(pattern)
        .map(|g| g.compile_matcher())
        .map_err(|e| format!("invalid glob `{pattern}`: {e}"))
}

/// Compile a regex. `Err` carries a model-facing explanation.
pub fn compile_regex(pattern: &str) -> Result<RegexMatcher, String> {
    RegexMatcher::new_line_matcher(pattern)
        .map_err(|e| format!("invalid regular expression `{pattern}`: {e}"))
}

/// Walk `root`, returning the files that pass `filter`, newest first.
///
/// Respects `.gitignore`/`.ignore` and skips hidden entries and `.git/`, so a
/// search never wades through `target/` or `node_modules/` — matching what a
/// developer means by "search the project".
pub fn candidates(root: &Path, filter: impl Fn(&Path) -> bool, limit: usize) -> Bounded<Candidate> {
    let cap = limit.min(MAX_CANDIDATES);
    let mut items: Vec<Candidate> = Vec::new();
    let mut clipped = false;

    let walk = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        // `.gitignore` is otherwise only honored inside a git repo. A komo
        // workspace need not be one, and the file still means what it says.
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();

    for entry in walk.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if !filter(entry.path()) {
            continue;
        }
        if items.len() >= cap {
            clipped = true;
            break;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        items.push(Candidate {
            path: entry.path().to_path_buf(),
            modified,
        });
    }

    // Newest first: when a search is over-broad, the files someone just touched
    // are the ones they meant. Ties fall back to path for a stable order.
    items.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    Bounded { items, clipped }
}

/// Search `paths` for `matcher`, stopping once `limit` matches are collected.
///
/// Binary files are skipped by content sniffing (`BinaryDetection::quit`), so a
/// stray `.bin` inside the tree can't inject a screen of noise.
pub fn search_files(paths: &[PathBuf], matcher: &RegexMatcher, limit: usize) -> Bounded<Match> {
    let mut items: Vec<Match> = Vec::new();
    let mut clipped = false;
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .build();

    'files: for path in paths {
        if items.len() >= limit {
            clipped = true;
            break;
        }
        // A per-file result is best-effort: an unreadable file is skipped, not
        // fatal — a search over a tree will always meet one eventually.
        let mut hit_limit = false;
        let _ = searcher.search_path(
            matcher,
            path,
            UTF8(|line, text| {
                items.push(Match {
                    path: path.clone(),
                    line,
                    text: text.trim_end().to_string(),
                });
                if items.len() >= limit {
                    hit_limit = true;
                    return Ok(false); // stop this file
                }
                Ok(true)
            }),
        );
        if hit_limit {
            clipped = true;
            break 'files;
        }
    }

    Bounded { items, clipped }
}

/// Render `path` relative to `root` when it is inside it, else absolutely. The
/// model gets short, paste-able paths without losing correctness for anything
/// outside the search root.
pub fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small tree: two rust files, one text file, one gitignored file.
    fn tree(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("komo_search_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored.txt\ntarget/\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {\n    hello();\n}\n").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn hello() {}\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "hello there\n").unwrap();
        std::fs::write(dir.join("ignored.txt"), "hello from an ignored file\n").unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("target/build.rs"), "hello from target\n").unwrap();
        dir
    }

    #[test]
    fn the_walk_honors_gitignore() {
        let dir = tree("gitignore");
        let found = candidates(&dir, |_| true, 100);
        let names: Vec<String> = found
            .items
            .iter()
            .map(|c| display_path(&dir, &c.path))
            .collect();
        assert!(names.contains(&"src/main.rs".to_string()), "{names:?}");
        assert!(
            !names.iter().any(|n| n.contains("ignored.txt")),
            "{names:?}"
        );
        assert!(!names.iter().any(|n| n.starts_with("target")), "{names:?}");
        // `.gitignore` itself is hidden, so it stays out too.
        assert!(!names.iter().any(|n| n == ".gitignore"), "{names:?}");
    }

    #[test]
    fn glob_filters_the_walk() {
        let dir = tree("glob");
        let matcher = compile_glob("**/*.rs").unwrap();
        let found = candidates(&dir, |p| matcher.is_match(p), 100);
        let names: Vec<String> = found
            .items
            .iter()
            .map(|c| display_path(&dir, &c.path))
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().all(|n| n.ends_with(".rs")));
    }

    #[test]
    fn grep_returns_line_numbers_and_stops_at_the_limit() {
        let dir = tree("grep");
        let paths: Vec<PathBuf> = candidates(&dir, |_| true, 100)
            .items
            .into_iter()
            .map(|c| c.path)
            .collect();
        let matcher = compile_regex("hello").unwrap();

        let all = search_files(&paths, &matcher, 100);
        assert!(all.items.len() >= 3, "{}", all.items.len());
        assert!(!all.clipped);
        let main_hit = all
            .items
            .iter()
            .find(|m| m.path.ends_with("main.rs"))
            .expect("main.rs matches");
        assert_eq!(main_hit.line, 2);
        assert_eq!(main_hit.text.trim(), "hello();");

        let capped = search_files(&paths, &matcher, 1);
        assert_eq!(capped.items.len(), 1);
        assert!(capped.clipped, "the limit must be reported, not hidden");
    }

    #[test]
    fn candidate_limit_is_reported() {
        let dir = tree("cap");
        let found = candidates(&dir, |_| true, 1);
        assert_eq!(found.items.len(), 1);
        assert!(found.clipped);
    }

    #[test]
    fn bad_patterns_explain_themselves() {
        assert!(compile_regex("(unclosed").is_err());
        assert!(compile_glob("a{b").is_err());
    }
}
