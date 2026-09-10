//! Notice that `config.toml` or `.env` changed, and say so.
//!
//! ## Why this is not a hot reload
//!
//! Most of komo's configuration cannot be swapped under a running process, and
//! the reasons are load-bearing rather than incidental: `[mcp.servers.*]` is
//! connected once at wiring and the catalog is immutable afterwards (a byte-
//! stable tool block is what keeps the provider's prompt cache valid);
//! `pyhost_enabled` is read *before* the executors exist, because it decides
//! whether `python` is registered at all; `[wiki] vault` and `[channels.*]`
//! decide what gets constructed. Making [`ConfigSnapshot`] mutable would
//! overturn "resolution happens once" — the rule the whole config crate is
//! built on — and buy a half-applied state the operator cannot reason about.
//!
//! So the thing worth fixing is not the restart. It is the **silence**: today
//! an edit takes effect at some unrelated future restart, and nothing says so
//! in between. This watch closes exactly that gap — it reports, and never
//! applies.
//!
//! ## The one re-read
//!
//! `komo-config`'s rule is that nothing re-reads `config.toml`. This is the
//! deliberate exception, and it holds the rule's actual purpose: the re-read
//! result is **compared and described, never installed**. The running process
//! keeps the snapshot it booted with, so there is still exactly one
//! authoritative resolution per process.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use async_trait::async_trait;
use tracing::{info, warn};

use super::{Maintenance, MaintenanceSummary};
use crate::notify::Notifier;
use std::sync::Arc;

/// What the operator is told, once per edit.
const TITLE: &str = "配置已改动";

pub struct ConfigWatchSweep {
    files: Vec<PathBuf>,
    /// The mtimes this process booted against. `None` for a file that did not
    /// exist — creating one later is a change like any other.
    seen: Mutex<Vec<Option<SystemTime>>>,
    notifier: Arc<dyn Notifier>,
}

impl ConfigWatchSweep {
    /// Watch `config.toml` and `.env` under `home`, from their state right now.
    pub fn new(home: &std::path::Path, notifier: Arc<dyn Notifier>) -> Self {
        let files = vec![home.join("config.toml"), home.join(".env")];
        let seen = files.iter().map(|p| mtime(p)).collect();
        Self {
            files,
            seen: Mutex::new(seen),
            notifier,
        }
    }

    /// Which watched files have moved since the last look, marking them seen.
    ///
    /// Marking before reporting is what makes this fire **once per edit**: a
    /// notification the operator has already had is noise, and an editor that
    /// writes twice in a second must not send two.
    fn changed(&self) -> Vec<String> {
        let mut seen = self.seen.lock().expect("config watch mutex");
        let mut moved = Vec::new();
        for (i, path) in self.files.iter().enumerate() {
            let now = mtime(path);
            if seen[i] != now {
                seen[i] = now;
                moved.push(
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string()),
                );
            }
        }
        moved
    }
}

fn mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

#[async_trait]
impl Maintenance for ConfigWatchSweep {
    async fn run(&self) -> anyhow::Result<MaintenanceSummary> {
        let changed = self.changed();
        if changed.is_empty() {
            return Ok(MaintenanceSummary::default());
        }
        let which = changed.join(" 和 ");

        // Parse the new content the way a boot would, so a typo is reported
        // while the operator is still at the keyboard rather than discovered
        // by a gateway that will not come back up.
        let candidate = komo_config::ConfigSnapshot::load();
        let body = match candidate.validate_gateway() {
            Ok(()) => {
                info!(files = %which, "config changed on disk; restart to apply");
                format!(
                    "{which} 已改动，内容能解析。当前进程仍在用启动时的配置——\
                     运行 `komo config reload` 应用（会校验后重启 gateway）。"
                )
            }
            Err(error) => {
                warn!(files = %which, %error, "config changed on disk but does not validate");
                format!(
                    "{which} 已改动，但**解析失败**：{error}\n\
                     没有应用任何东西，当前进程不受影响。修好之后再 `komo config reload`。"
                )
            }
        };
        // Best-effort, like every other sweep notification: failing to deliver
        // a notice must not fail the cycle and trip the breaker.
        if let Err(error) = self.notifier.notify(TITLE, &body).await {
            warn!(%error, "could not deliver the config-change notice");
        }
        Ok(MaintenanceSummary::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Silent;
    #[async_trait]
    impl Notifier for Silent {
        async fn notify(&self, _t: &str, _b: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("komo_cfgwatch_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_untouched_config_reports_nothing() {
        let home = tmp("quiet");
        std::fs::write(home.join("config.toml"), "model = \"x\"\n").unwrap();
        let watch = ConfigWatchSweep::new(&home, Arc::new(Silent));
        assert!(watch.changed().is_empty());
    }

    /// Once per edit, not once per tick: an operator who has already been told
    /// does not need telling every minute until they act.
    #[test]
    fn a_change_is_reported_once() {
        let home = tmp("once");
        let cfg = home.join("config.toml");
        std::fs::write(&cfg, "model = \"x\"\n").unwrap();
        let watch = ConfigWatchSweep::new(&home, Arc::new(Silent));

        // A same-second rewrite can carry the same mtime; force a distinct one.
        std::fs::write(&cfg, "model = \"y\"\n").unwrap();
        filetime_bump(&cfg);
        assert_eq!(watch.changed(), vec!["config.toml".to_string()]);
        assert!(watch.changed().is_empty(), "already reported");
    }

    /// A file that did not exist at boot and appears later is a change — that
    /// is how `.env` usually arrives.
    #[test]
    fn a_file_created_later_counts_as_a_change() {
        let home = tmp("created");
        std::fs::write(home.join("config.toml"), "model = \"x\"\n").unwrap();
        let watch = ConfigWatchSweep::new(&home, Arc::new(Silent));
        std::fs::write(home.join(".env"), "K=v\n").unwrap();
        assert_eq!(watch.changed(), vec![".env".to_string()]);
    }

    fn filetime_bump(path: &std::path::Path) {
        let later = SystemTime::now() + std::time::Duration::from_secs(2);
        let _ = std::fs::File::open(path).and_then(|f| f.set_modified(later));
    }
}
