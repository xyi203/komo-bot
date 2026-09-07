//! Operator commands over the skill store (`~/.komo/skills`) — roadmap §9.
//!
//! All of these are pure file operations on the store, so they work whether or
//! not the gateway is running (no db lock involved). The runtime
//! `SkillRegistry` re-scans the skill dirs on every query, so changes that
//! affect the agent's catalog (install / enable / disable) take effect on its
//! next agent `skill` tool list — no gateway restart.

use komo_infra::skills::FsSkillStore;
use std::{collections::HashSet, path::PathBuf};

fn store() -> FsSkillStore {
    FsSkillStore::new(FsSkillStore::default_root())
}

fn shared_store() -> Option<FsSkillStore> {
    dirs::home_dir().map(|home| FsSkillStore::new(home.join(".agents/skills")))
}

struct ReadableSkill {
    skill: crate::domain::skill::Skill,
    status: &'static str,
    path: PathBuf,
}

fn find_readable_skill(
    name: &str,
    managed: &FsSkillStore,
    shared: Option<&FsSkillStore>,
) -> Option<ReadableSkill> {
    if let Some(skill) = managed.find_active(name) {
        return Some(ReadableSkill {
            skill,
            status: "active",
            path: managed.active_path(name),
        });
    }
    shared.and_then(|shared| {
        shared.find_active(name).map(|skill| ReadableSkill {
            skill,
            status: "shared (read-only)",
            path: shared.active_path(name),
        })
    })
}

/// List Komo-managed skills plus the shared skills already installed for local
/// agents under `~/.agents/skills`. Shared skills are discoverable and
/// inspectable, but enable/disable only mutates the managed store.
pub fn list() -> anyhow::Result<()> {
    let managed = store();
    let shared = shared_store();
    let active = managed.list_active();
    let managed_names = active
        .iter()
        .map(|skill| skill.name.as_str())
        .collect::<HashSet<_>>();
    let shared_skills = shared
        .as_ref()
        .map(FsSkillStore::list_active)
        .unwrap_or_default()
        .into_iter()
        .filter(|skill| !managed_names.contains(skill.name.as_str()))
        .collect::<Vec<_>>();

    if active.is_empty() && shared_skills.is_empty() {
        println!(
            "No skills in {} or ~/.agents/skills.",
            managed.root().display()
        );
        return Ok(());
    }
    if !active.is_empty() {
        println!("managed ({}):", managed.root().display());
        for skill in &active {
            print_skill_line(skill, "  ");
        }
    }
    if !shared_skills.is_empty() {
        let root = shared
            .as_ref()
            .expect("shared skills came from this store")
            .root();
        println!("shared ({}; read-only):", root.display());
        for skill in &shared_skills {
            print_skill_line(skill, "  ");
        }
    }
    Ok(())
}

fn print_skill_line(skill: &crate::domain::skill::Skill, prefix: &str) {
    let off = if skill.disabled { " [disabled]" } else { "" };
    println!(
        "{prefix}{}{}  {}",
        skill.name,
        off,
        oneline(&skill.description, 80)
    );
}

fn oneline(value: &str, max: usize) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.chars().count() <= max {
        return value;
    }
    format!(
        "{}…",
        value
            .chars()
            .take(max.saturating_sub(1))
            .collect::<String>()
    )
}

const RELOAD_HINT: &str = "Takes effect on the agent's next `skill` list (no restart needed).";

/// Install a skill from a git repo (`owner/repo[/subpath]`, a GitHub URL, or any
/// `*.git`/`git@…` URL) or a raw `SKILL.md` URL, straight into the active store.
/// Pure file+network ops — works while the gateway holds the db lock; the live
/// registry means the running agent sees it on its next `skill` list, no restart.
pub async fn install(source: &str) -> anyhow::Result<()> {
    let installed = komo_infra::skill_install::install(&store(), source).await?;
    let files = if installed.files == 1 {
        "1 file".to_string()
    } else {
        format!("{} files", installed.files)
    };
    println!(
        "Installed `{}` ({files}) → {}",
        installed.name,
        installed.path.display()
    );
    if !installed.description.is_empty() {
        println!("  {}", installed.description);
    }
    println!("Active now; the agent picks it up on its next `skill` list (no restart needed).");
    Ok(())
}

/// Enable or disable an active skill without deleting it: disabled skills stay
/// on disk and inspectable but are hidden from the model's catalog.
pub fn set_enabled(name: &str, enabled: bool) -> anyhow::Result<()> {
    let skill = store().set_disabled(name, !enabled)?;
    if skill.disabled {
        println!(
            "Disabled `{}` — kept on disk, hidden from the agent. {RELOAD_HINT}",
            skill.name
        );
    } else {
        println!("Enabled `{}`. {RELOAD_HINT}", skill.name);
    }
    Ok(())
}

/// One skill in full: status, provenance, file path, and the instruction body.
pub fn inspect(name: &str) -> anyhow::Result<()> {
    let managed = store();
    let shared = shared_store();
    let found = find_readable_skill(name, &managed, shared.as_ref()).ok_or_else(|| {
        anyhow::anyhow!(
            "no skill named `{name}` in {} or ~/.agents/skills",
            managed.root().display()
        )
    })?;
    let skill = found.skill;

    println!("skill      {}", skill.name);
    let mut state = found.status.to_string();
    if skill.disabled {
        state.push_str(" [disabled]");
    }
    println!("status     {state}");
    println!("source     {}", skill.source);
    println!("path       {}", found.path.display());
    if !skill.description.is_empty() {
        println!("describes  {}", skill.description);
    }
    // Offer gating trims the agent's always-on catalog only, so say so — an
    // operator seeing `platforms` here should not think the skill is unusable.
    if !skill.platforms.is_empty() || !skill.requires_tools.is_empty() {
        let mut gates = Vec::new();
        if !skill.platforms.is_empty() {
            gates.push(format!("platforms {}", skill.platforms.join("/")));
        }
        if !skill.requires_tools.is_empty() {
            gates.push(format!("needs tool {}", skill.requires_tools.join(" + ")));
        }
        println!(
            "offered    {} — catalog only; always loadable by name",
            gates.join(", ")
        );
    }
    println!("\n{}", skill.instructions);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_skill(root: &std::path::Path, name: &str, body: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test\n---\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn readable_skill_includes_shared_agents_store_with_managed_precedence() {
        let base = std::env::temp_dir().join("komo_cli_shared_skills");
        let _ = fs::remove_dir_all(&base);
        let managed = FsSkillStore::new(base.join("managed"));
        let shared = FsSkillStore::new(base.join("shared"));
        write_skill(shared.root(), "only-shared", "shared body");
        write_skill(shared.root(), "duplicate", "shared version");
        write_skill(managed.root(), "duplicate", "managed version");

        let found = find_readable_skill("only-shared", &managed, Some(&shared)).unwrap();
        assert_eq!(found.status, "shared (read-only)");
        assert!(found.skill.instructions.contains("shared body"));

        let found = find_readable_skill("duplicate", &managed, Some(&shared)).unwrap();
        assert_eq!(found.status, "active");
        assert!(found.skill.instructions.contains("managed version"));

        assert!(find_readable_skill("absent", &managed, Some(&shared)).is_none());

        let _ = fs::remove_dir_all(&base);
    }
}
