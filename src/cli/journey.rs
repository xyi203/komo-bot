//! `komo journey` — a read-only timeline of what komo has *learned* over
//! time: long-term memories (born, promoted, archived) and governed skills
//! (proposed as candidates, activated).
//!
//! This is the observability surface over komo's two learning subsystems
//! (memory §5, skills §9). It is deliberately **not** an execution log — that
//! is `komo run list`. It composes existing reads (the memory loader, which
//! routes to a running gateway over HTTP, and the file-backed skill store,
//! which never needs the db lock), so it adds no new api endpoint or schema.
//!
//! A caveat on fidelity: the stores keep `created_at` + `updated_at`, not a
//! full status-transition log, so "promoted"/"archived" events are inferred
//! from the current status plus `updated_at > created_at`. Skill event times
//! come from the `SKILL.md` file mtimes (proposal / activation).

use komo_infra::skills::FsSkillStore;
use std::path::Path;

use crate::{
    cli::{inspect::local_time, memory},
    domain::{
        memory::{Memory, MemoryStatus},
        skill::Skill,
    },
    services::operator_control::OperatorControl,
};

/// One dated thing komo learned (or forgot). Kept small and owned so the
/// assembly logic is a pure, testable function over in-memory values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Event {
    /// Unix seconds — when it happened.
    at: i64,
    /// Emoji + short label for the subsystem (e.g. "🧠 记忆", "🛠 技能").
    icon: &'static str,
    /// What happened, one word (诞生 / 转正 / 归档 / 候选 / 活跃).
    verb: &'static str,
    /// The human-readable subject line.
    detail: String,
    /// Trailing annotation (status, recall count, provenance) — may be empty.
    tail: String,
}

pub async fn journey(
    control: &OperatorControl,
    limit: usize,
    since: Option<String>,
) -> anyhow::Result<()> {
    let cutoff = match since.as_deref() {
        Some(date) => Some(crate::cli::app::parse_local_date(date)?),
        None => None,
    };

    let mut events = memory_events(&memory::load_all(control).await?);
    events.extend(skill_events(&FsSkillStore::new(
        FsSkillStore::default_root(),
    )));

    let shown = finalize(events, cutoff, limit);
    if shown.is_empty() {
        println!("(nothing learned yet)");
        return Ok(());
    }
    for e in &shown {
        let tail = if e.tail.is_empty() {
            String::new()
        } else {
            format!("   {}", e.tail)
        };
        println!(
            "{}  {}  {}   {}{}",
            local_time(e.at),
            e.icon,
            e.verb,
            e.detail,
            tail
        );
    }
    println!(
        "\n({} event(s); timings inferred from created/updated + file mtimes. \
         `komo memory list` / `komo skills list` show current state.)",
        shown.len()
    );
    Ok(())
}

/// Flatten memories into born / promoted / archived events (pure).
pub(crate) fn memory_events(memories: &[Memory]) -> Vec<Event> {
    let mut events = Vec::new();
    for m in memories {
        // A rejected memory was discarded by the operator; it isn't part of the
        // "learned" story, so skip it entirely.
        if m.status == MemoryStatus::Rejected {
            continue;
        }
        let subject = format!("{} · \"{}\"", m.kind.as_str(), truncate(&m.content, 60));

        // Birth: every surviving memory started as a candidate/observation.
        let born_tail = match m.status {
            MemoryStatus::Candidate => "[candidate]".to_string(),
            _ => String::new(),
        };
        events.push(Event {
            at: m.created_at,
            icon: "🧠 记忆",
            verb: "诞生",
            detail: subject.clone(),
            tail: born_tail,
        });

        // A later status change (promotion by dreaming/operator, or archival)
        // shows only if `updated_at` actually moved past creation.
        if m.updated_at > m.created_at {
            let (verb, show) = match m.status {
                MemoryStatus::Active => ("转正", true),
                MemoryStatus::Archived => ("归档", true),
                _ => ("", false),
            };
            if show {
                let tail = if m.recall_count > 0 {
                    format!("recalled {}×", m.recall_count)
                } else {
                    String::new()
                };
                events.push(Event {
                    at: m.updated_at,
                    icon: "🧠 记忆",
                    verb,
                    detail: subject,
                    tail,
                });
            }
        }
    }
    events
}

/// Flatten the skill store into activation events, timed by the `SKILL.md` file
/// mtime. Reads files only — no db lock, so it works while the gateway runs
/// (same as `komo skills`).
fn skill_events(store: &FsSkillStore) -> Vec<Event> {
    let mut events = Vec::new();
    for s in store.list_active() {
        if let Some(at) = mtime(&store.active_path(&s.name)) {
            events.push(skill_event(at, "活跃", &s));
        }
    }
    events
}

fn skill_event(at: i64, verb: &'static str, s: &Skill) -> Event {
    let detail = if s.description.is_empty() {
        s.name.clone()
    } else {
        format!("{} — {}", s.name, truncate(&s.description, 50))
    };
    Event {
        at,
        icon: "🛠 技能",
        verb,
        detail,
        tail: format!("({})", s.source),
    }
}

/// File mtime as unix seconds, or `None` if the file is missing/unreadable.
fn mtime(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    i64::try_from(secs).ok()
}

/// Sort newest-first, drop anything before `cutoff`, cap at `limit` (pure).
pub(crate) fn finalize(mut events: Vec<Event>, cutoff: Option<i64>, limit: usize) -> Vec<Event> {
    if let Some(cutoff) = cutoff {
        events.retain(|e| e.at >= cutoff);
    }
    events.sort_by_key(|e| std::cmp::Reverse(e.at));
    events.truncate(limit);
    events
}

/// Truncate to `max` chars (char-safe), appending `…` when cut.
fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        return s;
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::memory::{Memory, MemoryKind};

    fn mem(
        content: &str,
        status: MemoryStatus,
        created: i64,
        updated: i64,
        recalls: i64,
    ) -> Memory {
        let mut m = Memory::new(MemoryKind::Fact, content);
        m.status = status;
        m.created_at = created;
        m.updated_at = updated;
        m.recall_count = recalls;
        m
    }

    #[test]
    fn candidate_emits_only_a_birth_event() {
        let events = memory_events(&[mem("a", MemoryStatus::Candidate, 100, 100, 0)]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].verb, "诞生");
        assert_eq!(events[0].tail, "[candidate]");
    }

    #[test]
    fn promoted_memory_emits_birth_and_promotion() {
        let events = memory_events(&[mem("a", MemoryStatus::Active, 100, 200, 4)]);
        assert_eq!(events.len(), 2);
        // born at creation, promoted at update.
        let promo = events.iter().find(|e| e.verb == "转正").unwrap();
        assert_eq!(promo.at, 200);
        assert_eq!(promo.tail, "recalled 4×");
        assert!(events.iter().any(|e| e.verb == "诞生" && e.at == 100));
    }

    #[test]
    fn active_from_birth_has_no_promotion_event() {
        // updated_at == created_at: nothing changed after creation.
        let events = memory_events(&[mem("a", MemoryStatus::Active, 100, 100, 0)]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].verb, "诞生");
    }

    #[test]
    fn archived_memory_shows_forgetting() {
        let events = memory_events(&[mem("a", MemoryStatus::Archived, 100, 300, 0)]);
        assert!(events.iter().any(|e| e.verb == "归档" && e.at == 300));
    }

    #[test]
    fn rejected_memory_is_skipped() {
        let events = memory_events(&[mem("a", MemoryStatus::Rejected, 100, 200, 0)]);
        assert!(events.is_empty());
    }

    #[test]
    fn finalize_sorts_desc_filters_and_caps() {
        let ev = |at| Event {
            at,
            icon: "🧠 记忆",
            verb: "诞生",
            detail: String::new(),
            tail: String::new(),
        };
        let out = finalize(vec![ev(100), ev(300), ev(200)], Some(150), 2);
        // 100 filtered out by cutoff; remaining sorted newest-first, capped at 2.
        assert_eq!(out.iter().map(|e| e.at).collect::<Vec<_>>(), vec![300, 200]);
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("héllo world", 5), "héllo…");
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a\nb", 10), "a b");
    }
}
