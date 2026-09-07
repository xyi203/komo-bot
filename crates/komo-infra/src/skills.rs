//! Filesystem-backed skill store — the single source of truth for skills.
//!
//! Skills are durable personal data (peers of memory), so they live as
//! `SKILL.md` files under `~/.komo/skills/<name>/`, not in the disposable
//! `state.db`. Files are editable, shareable, and lock-free: every operator
//! action works while the gateway holds the Turso db lock.
//!
//! Layout under the root: `<name>/SKILL.md` — an **active** skill, loaded into
//! the runtime `SkillRegistry` (the root is one of its scan directories).
//! Skills are written and installed by a human; nothing here writes one on the
//! agent's behalf.

use std::fs;
use std::path::{Path, PathBuf};

use tracing::{debug, warn};

use komo_core::domain::skill::{Skill, valid_skill_name};

/// Build the runtime's ordered skill search path. Earlier directories win when
/// two skills share a name. `~/.agents/skills` is the shared, read-only skill
/// collection used by Codex and other local agents; Komo discovers it without
/// taking ownership of its files.
pub fn runtime_skill_dirs(
    configured: &[PathBuf],
    workspace_root: &Path,
    governed_root: &Path,
    user_home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs = configured.to_vec();
    dirs.push(workspace_root.join("skills"));
    dirs.push(workspace_root.join(".claude/skills"));
    dirs.push(governed_root.to_path_buf());
    if let Some(home) = user_home {
        dirs.push(home.join(".agents/skills"));
        dirs.push(home.join(".claude/skills"));
    }
    dirs
}

pub struct FsSkillStore {
    root: PathBuf,
}

impl FsSkillStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The komo-owned skills home: `~/.komo/skills`.
    pub fn default_root() -> PathBuf {
        komo_config::komo_home().join("skills")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn active_path(&self, name: &str) -> PathBuf {
        self.root.join(name).join("SKILL.md")
    }

    /// Active skills (the governed subset the registry loads from this root —
    /// workspace/`~/.claude` skill dirs are governed by their own repos).
    pub fn list_active(&self) -> Vec<Skill> {
        scan_dir(&self.root)
    }

    pub fn find_active(&self, name: &str) -> Option<Skill> {
        valid_skill_name(name)
            .then(|| read_skill(&self.active_path(name)))
            .flatten()
    }

    /// Flip an active skill's `disabled` flag (operator-only path).
    pub fn set_disabled(&self, name: &str, on: bool) -> anyhow::Result<Skill> {
        self.update_active(name, |s| s.disabled = on)
    }

    fn update_active(&self, name: &str, mutate: impl FnOnce(&mut Skill)) -> anyhow::Result<Skill> {
        let Some(mut skill) = self.find_active(name) else {
            anyhow::bail!("no active skill named `{name}` in {}", self.root.display());
        };
        mutate(&mut skill);
        self.write_active(&skill)?;
        Ok(skill)
    }

    fn write_active(&self, skill: &Skill) -> anyhow::Result<()> {
        let path = self.active_path(&skill.name);
        fs::create_dir_all(path.parent().expect("skill path has a parent"))?;
        fs::write(&path, render(skill))?;
        Ok(())
    }

    /// Install a skill **directory** (its `SKILL.md` plus any supporting files —
    /// scripts, `references/`, etc.) as an **active** skill, copying the whole
    /// tree. This is the only write path (operator `komo skills install` + the
    /// approved `skill` tool `install` action) and it overwrites an existing
    /// active skill of the same name. Returns the parsed skill and the number of
    /// files copied.
    pub fn install_active_dir(&self, src_dir: &Path) -> anyhow::Result<(Skill, usize)> {
        let skill = read_skill(&src_dir.join("SKILL.md")).ok_or_else(|| {
            anyhow::anyhow!(
                "no valid SKILL.md (with frontmatter) in {}",
                src_dir.display()
            )
        })?;
        if !valid_skill_name(&skill.name) {
            anyhow::bail!(
                "invalid skill name `{}` (letters, digits, `-`/`_`/`.` only)",
                skill.name
            );
        }
        let dest = self.root.join(&skill.name);
        if dest.exists() {
            fs::remove_dir_all(&dest)?;
        }
        let files = copy_dir_all(src_dir, &dest)?;
        Ok((skill, files))
    }
}

fn read_skill(path: &Path) -> Option<Skill> {
    let content = fs::read_to_string(path).ok()?;
    Skill::parse(&content)
}

/// Recursively copy `src` into `dst` (created if absent), skipping any nested
/// `.git` directory (defensive — install stages from a subdir, not a clone
/// root, but a vendored skill could still carry one). Returns the number of
/// regular files copied.
fn copy_dir_all(src: &Path, dst: &Path) -> anyhow::Result<usize> {
    fs::create_dir_all(dst)?;
    let mut count = 0;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            count += copy_dir_all(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
            count += 1;
        }
    }
    Ok(count)
}

/// Scan `dir` for `<name>/SKILL.md` entries (same shape as the runtime
/// registry's scan). Dot-prefixed entries never match: dot names are rejected
/// at parse level too.
fn scan_dir(dir: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        debug!(?dir, "no skills directory; skipped");
        return skills;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let manifest = entry.path().join("SKILL.md");
        if !manifest.is_file() {
            continue;
        }
        match read_skill(&manifest) {
            Some(skill) => skills.push(skill),
            None => warn!(?manifest, "SKILL.md missing valid frontmatter; skipped"),
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Render a skill back to `SKILL.md`: identity frontmatter, the `disabled` flag
/// only when set (hand-written files stay minimal), then the body.
fn render(skill: &Skill) -> String {
    let mut front = format!("---\nname: {}\n", skill.name);
    if !skill.description.is_empty() {
        front.push_str(&format!("description: {}\n", skill.description));
    }
    if skill.source != komo_core::domain::skill::SOURCE_USER {
        front.push_str(&format!("source: {}\n", skill.source));
    }
    if skill.disabled {
        front.push_str("disabled: true\n");
    }
    // Round-tripped so an operator action that rewrites the file (enable /
    // disable) can never silently drop a skill's offer gating.
    if !skill.platforms.is_empty() {
        front.push_str(&format!("platforms: [{}]\n", skill.platforms.join(", ")));
    }
    if !skill.requires_tools.is_empty() {
        front.push_str(&format!(
            "requires_tools: [{}]\n",
            skill.requires_tools.join(", ")
        ));
    }
    format!("{front}---\n\n{}\n", skill.instructions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dirs_include_user_agents_skills() {
        let configured = vec![PathBuf::from("/configured/skills")];
        let dirs = runtime_skill_dirs(
            &configured,
            Path::new("/workspace"),
            Path::new("/komo/skills"),
            Some(Path::new("/user")),
        );
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/configured/skills"),
                PathBuf::from("/workspace/skills"),
                PathBuf::from("/workspace/.claude/skills"),
                PathBuf::from("/komo/skills"),
                PathBuf::from("/user/.agents/skills"),
                PathBuf::from("/user/.claude/skills"),
            ]
        );
    }

    fn store(name: &str) -> FsSkillStore {
        let root = std::env::temp_dir().join(name);
        let _ = fs::remove_dir_all(&root);
        FsSkillStore::new(root)
    }

    /// A staging directory holding one `SKILL.md`, as `install` is handed.
    fn staged(tag: &str, name: &str, body: &str) -> PathBuf {
        let src = std::env::temp_dir().join(format!("komo_install_src_{tag}"));
        let _ = fs::remove_dir_all(&src);
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: does {name}\n---\n{body}"),
        )
        .unwrap();
        src
    }

    #[test]
    fn install_active_dir_copies_the_whole_skill_tree() {
        let store = store("komo_skillstore_install");
        // A multi-file skill: SKILL.md plus a supporting script.
        let src = staged("tree", "mediarr", "Do media things.");
        fs::create_dir_all(src.join("scripts")).unwrap();
        fs::write(src.join("scripts").join("run.sh"), "echo hi\n").unwrap();

        let (skill, files) = store.install_active_dir(&src).unwrap();
        assert_eq!(skill.name, "mediarr");
        assert_eq!(files, 2);
        // Active + supporting file both landed in the store.
        assert!(store.find_active("mediarr").is_some());
        assert!(store.root().join("mediarr/scripts/run.sh").is_file());
        assert_eq!(store.list_active().len(), 1);

        let _ = fs::remove_dir_all(&src);
    }

    #[test]
    fn install_rejects_a_dir_without_a_valid_manifest() {
        let store = store("komo_skillstore_install_nomanifest");
        let src = std::env::temp_dir().join("komo_install_src_empty");
        let _ = fs::remove_dir_all(&src);
        fs::create_dir_all(&src).unwrap();
        assert!(store.install_active_dir(&src).is_err());
        let _ = fs::remove_dir_all(&src);
    }

    #[test]
    fn disable_and_enable_roundtrip() {
        let store = store("komo_skillstore_disable");
        let src = staged("disable", "sync-cal", "How to sync-cal.");
        store.install_active_dir(&src).unwrap();

        let s = store.set_disabled("sync-cal", true).unwrap();
        assert!(s.disabled);
        assert!(store.find_active("sync-cal").unwrap().disabled);
        let s = store.set_disabled("sync-cal", false).unwrap();
        assert!(!s.disabled);
        // The body round-trips through render/parse.
        assert!(
            store
                .find_active("sync-cal")
                .unwrap()
                .instructions
                .contains("How to sync-cal.")
        );

        let _ = fs::remove_dir_all(&src);
    }

    #[test]
    fn set_disabled_refuses_an_unknown_skill() {
        let store = store("komo_skillstore_disable_missing");
        assert!(store.set_disabled("nope", true).is_err());
    }
}
