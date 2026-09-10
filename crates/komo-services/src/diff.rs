//! Unified diffs for the mutating file tools (`edit`, `write`).
//!
//! Two audiences, one computation: the model gets `+N -M` counts inline (a
//! cheap, verifiable confirmation of what its edit actually did), while the full
//! patch goes into `ToolOutput::structured` for the run ledger and the UI —
//! where it costs no tokens.

use similar::TextDiff;

/// A change, measured and rendered.
pub struct Diff {
    /// Lines added.
    pub additions: usize,
    /// Lines removed.
    pub deletions: usize,
    /// Unified diff, `git apply`-shaped (3 lines of context).
    pub patch: String,
}

/// Diff `before` against `after`, labeling both sides `name`.
pub fn unified(name: &str, before: &str, after: &str) -> Diff {
    let diff = TextDiff::from_lines(before, after);
    let mut additions = 0;
    let mut deletions = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => additions += 1,
            similar::ChangeTag::Delete => deletions += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    let patch = diff
        .unified_diff()
        .context_radius(3)
        .header(name, name)
        .to_string();
    Diff {
        additions,
        deletions,
        patch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_renders_a_one_line_change() {
        let d = unified("a.rs", "one\ntwo\nthree\n", "one\nTWO\nthree\n");
        assert_eq!((d.additions, d.deletions), (1, 1));
        assert!(d.patch.contains("-two"), "{}", d.patch);
        assert!(d.patch.contains("+TWO"), "{}", d.patch);
        assert!(d.patch.contains("a.rs"), "{}", d.patch);
        // Unchanged context is carried, so the patch is applyable.
        assert!(d.patch.contains(" one"), "{}", d.patch);
    }

    #[test]
    fn pure_insertion_counts_only_additions() {
        let d = unified("a", "one\n", "one\ntwo\n");
        assert_eq!((d.additions, d.deletions), (1, 0));
    }

    #[test]
    fn no_change_is_an_empty_patch() {
        let d = unified("a", "same\n", "same\n");
        assert_eq!((d.additions, d.deletions), (0, 0));
        assert!(d.patch.is_empty(), "{}", d.patch);
    }
}
