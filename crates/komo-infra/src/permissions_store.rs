//! Persisted "allow this from now on" approvals — `~/.komo/permissions.json`.
//!
//! komo's approval memory used to end with the process: `/approve session`
//! cached a scope key until the gateway restarted, and anything longer-lived had
//! to be hand-written as a `[[policy.rule]]`. This is the third answer — `a` at
//! the prompt — accumulating **narrow allow rules** as the operator grants them.
//!
//! Two deliberate choices:
//!
//! - **Its own file, not `state.db`.** That db is disposable (delete it to
//!   reset); a grant the operator made is durable personal data, like memory.db /
//!   cron.db.
//! - **JSON, not a fourth db.** There are a handful of entries and the operator
//!   should be able to read and delete them with an editor. The schema is
//!   deliberately isomorphic with `[[policy.rule]]`, so a saved entry is just "a
//!   policy allow rule that accumulated at runtime" and shares the same matching
//!   code — `komo policy check` can explain it the same way.
//!
//! The in-memory list is [shared](SavedRules) with every `Policy` clone, so an
//! entry saved at a prompt applies to the very next decision. The file is the
//! record; the `Arc` is the live view.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use tracing::warn;

use komo_core::domain::policy::{
    Access, Category, Effect, Matcher, Rule, SavedRules, category_str, matcher_str,
};

/// Bumped only for a breaking shape change; an unknown version is refused rather
/// than half-read, so a downgrade can't silently drop the operator's grants.
const VERSION: u32 = 1;

const FILE_NAME: &str = "permissions.json";

#[derive(Serialize, Deserialize)]
struct Document {
    version: u32,
    #[serde(default)]
    entries: Vec<Entry>,
}

/// One saved grant, in the same shape as a `[[policy.rule]]` table.
#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    category: String,
    #[serde(rename = "match")]
    matcher: String,
    value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    channel: Option<String>,
    /// RFC 3339, for the operator reading the file — never used in matching.
    created_at: String,
    /// How it got here (`approval` today). Provenance, like a skill's `source`.
    source: String,
}

pub struct PermissionsStore {
    path: PathBuf,
    rules: SavedRules,
}

impl PermissionsStore {
    /// Open the store under `home`, reading whatever is already saved. A missing
    /// file is the normal case (nothing granted yet); an unreadable or
    /// wrong-version one is reported and treated as empty — a broken file must
    /// not stop the gateway, and the *safe* direction of "can't read your grants"
    /// is to ask again.
    pub fn load(home: &Path) -> Self {
        let path = home.join(FILE_NAME);
        let rules = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Document>(&text) {
                Ok(doc) if doc.version == VERSION => {
                    doc.entries.iter().filter_map(to_rule).collect()
                }
                Ok(doc) => {
                    warn!(
                        path = %path.display(), found = doc.version, expected = VERSION,
                        "permissions.json has an unknown version; ignoring it (approvals will be asked again)"
                    );
                    Vec::new()
                }
                Err(error) => {
                    warn!(%error, path = %path.display(), "permissions.json is unreadable; ignoring it");
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        };
        Self {
            path,
            rules: Arc::new(RwLock::new(rules)),
        }
    }

    /// The live list to hand `Policy::with_saved`.
    pub fn rules(&self) -> SavedRules {
        self.rules.clone()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many grants are saved (for `komo doctor`).
    pub fn len(&self) -> usize {
        self.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A snapshot in evaluation order, for `komo policy saved list`.
    pub fn list(&self) -> Vec<Rule> {
        self.read()
    }

    /// Remember `rule`, in memory and on disk. A duplicate is a no-op, so
    /// answering `a` twice for the same thing doesn't grow the file.
    ///
    /// Returns whether anything was added. A write failure is logged and the
    /// in-memory grant still stands for this process — the user asked for it, and
    /// failing the tool call over a persistence problem would be the wrong end to
    /// break.
    pub fn remember(&self, rule: Rule, now: &str) -> bool {
        {
            let mut rules = self.rules.write().unwrap_or_else(|e| e.into_inner());
            if rules.iter().any(|r| same_grant(r, &rule)) {
                return false;
            }
            rules.push(rule);
        }
        self.persist(now);
        true
    }

    /// Drop the entry at `index` (as numbered by [`list`](Self::list)). Returns
    /// the removed rule, or `None` when the index is out of range.
    pub fn forget(&self, index: usize, now: &str) -> Option<Rule> {
        let removed = {
            let mut rules = self.rules.write().unwrap_or_else(|e| e.into_inner());
            (index < rules.len()).then(|| rules.remove(index))
        };
        if removed.is_some() {
            self.persist(now);
        }
        removed
    }

    /// Drop every saved grant, returning how many went.
    pub fn forget_all(&self, now: &str) -> usize {
        let count = {
            let mut rules = self.rules.write().unwrap_or_else(|e| e.into_inner());
            let count = rules.len();
            rules.clear();
            count
        };
        if count > 0 {
            self.persist(now);
        }
        count
    }

    fn read(&self) -> Vec<Rule> {
        self.rules.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Write the whole document. Rewriting beats appending: the file is tiny, and
    /// one code path for add/remove can't leave the two out of step.
    fn persist(&self, now: &str) {
        let doc = Document {
            version: VERSION,
            entries: self.read().iter().map(|r| from_rule(r, now)).collect(),
        };
        let write = serde_json::to_string_pretty(&doc)
            .map_err(|e| e.to_string())
            .and_then(|text| std::fs::write(&self.path, text + "\n").map_err(|e| e.to_string()));
        if let Err(error) = write {
            warn!(
                %error, path = %self.path.display(),
                "could not persist permissions.json; the grant holds for this process only"
            );
        }
    }
}

/// Two grants are the same when they match the same things — timestamps and
/// provenance don't count.
fn same_grant(a: &Rule, b: &Rule) -> bool {
    a.category == b.category
        && a.matcher == b.matcher
        && a.value == b.value
        && a.access == b.access
        && a.channels == b.channels
}

fn to_rule(entry: &Entry) -> Option<Rule> {
    Some(Rule {
        channels: entry.channel.clone().map(|c| vec![c]),
        category: Category::parse(&entry.category)?,
        matcher: Matcher::parse(&entry.matcher)?,
        value: entry.value.clone(),
        access: match &entry.access {
            Some(a) => Some(Access::parse(a)?),
            None => None,
        },
        // The file holds grants only; nothing here can deny, and neither flag is
        // representable — the engine refuses to read a saved entry for a
        // dangerous or unattended action regardless of what a hand-edited file
        // claims.
        effect: Effect::Allow,
        include_dangerous: false,
        unattended: false,
    })
}

fn from_rule(rule: &Rule, now: &str) -> Entry {
    Entry {
        category: category_str(rule.category).to_string(),
        matcher: matcher_str(rule.matcher).to_string(),
        value: rule.value.clone(),
        access: rule.access.map(|a| {
            match a {
                Access::Read => "read",
                Access::Write => "write",
            }
            .to_string()
        }),
        channel: rule.channels.as_ref().and_then(|c| c.first().cloned()),
        created_at: now.to_string(),
        source: "approval".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_core::domain::approval::ActionRef;

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("komo_perms_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn shell_rule(cmd: &str) -> Rule {
        Rule::narrowest_for(
            &ActionRef::Shell {
                command: cmd.to_string(),
            },
            "cli",
        )
        .unwrap()
    }

    #[test]
    fn a_grant_survives_a_reload() {
        let dir = home("roundtrip");
        let store = PermissionsStore::load(&dir);
        assert!(store.is_empty());
        assert!(store.remember(shell_rule("cargo build"), "2026-07-28T10:00:00Z"));

        // A fresh store — as the next gateway start would build it.
        let reloaded = PermissionsStore::load(&dir);
        let rules = reloaded.list();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].value, "cargo ");
        assert_eq!(rules[0].category, Category::Shell);
        assert_eq!(rules[0].channels, Some(vec!["cli".to_string()]));
        assert_eq!(rules[0].effect, Effect::Allow);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_grant_is_not_saved_twice() {
        let dir = home("dedup");
        let store = PermissionsStore::load(&dir);
        assert!(store.remember(shell_rule("cargo build"), "t"));
        assert!(
            !store.remember(shell_rule("cargo test"), "t"),
            "both narrow to `cargo `"
        );
        assert_eq!(store.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forget_removes_one_or_all() {
        let dir = home("forget");
        let store = PermissionsStore::load(&dir);
        store.remember(shell_rule("cargo build"), "t");
        store.remember(shell_rule("git status"), "t");
        assert_eq!(store.len(), 2);

        let removed = store.forget(0, "t").expect("index 0 exists");
        assert_eq!(removed.value, "cargo ");
        assert_eq!(PermissionsStore::load(&dir).len(), 1, "persisted");
        assert!(store.forget(9, "t").is_none());

        assert_eq!(store.forget_all("t"), 1);
        assert!(PermissionsStore::load(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A grant is durable personal data: a file komo can't parse must be
    /// reported and ignored, never half-applied.
    #[test]
    fn an_unreadable_or_future_file_is_ignored_rather_than_half_read() {
        let dir = home("broken");
        std::fs::write(dir.join(FILE_NAME), "{ not json").unwrap();
        assert!(PermissionsStore::load(&dir).is_empty());

        std::fs::write(
            dir.join(FILE_NAME),
            r#"{"version":99,"entries":[{"category":"shell","match":"prefix","value":"rm ","created_at":"t","source":"x"}]}"#,
        )
        .unwrap();
        assert!(PermissionsStore::load(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hand-edited file can't smuggle in anything the prompt couldn't grant:
    /// unknown fields are ignored and every entry loads as a plain allow.
    #[test]
    fn a_hand_edited_entry_cannot_widen_itself() {
        let dir = home("handedit");
        std::fs::write(
            dir.join(FILE_NAME),
            r#"{"version":1,"entries":[{"category":"shell","match":"prefix","value":"rm ",
                 "created_at":"t","source":"hand","effect":"deny","include_dangerous":true,
                 "unattended":true}]}"#,
        )
        .unwrap();
        let rules = PermissionsStore::load(&dir).list();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].effect, Effect::Allow);
        assert!(!rules[0].include_dangerous);
        assert!(!rules[0].unattended);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unparseable_entry_is_dropped_without_taking_the_others() {
        let dir = home("partial");
        std::fs::write(
            dir.join(FILE_NAME),
            r#"{"version":1,"entries":[
                 {"category":"nonsense","match":"prefix","value":"x","created_at":"t","source":"a"},
                 {"category":"shell","match":"prefix","value":"cargo ","created_at":"t","source":"a"}
               ]}"#,
        )
        .unwrap();
        let rules = PermissionsStore::load(&dir).list();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].value, "cargo ");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
