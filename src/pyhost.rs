//! The Python plugin host's lifecycle: `$KOMO_HOME/plugins/*.py` become
//! callable tools.
//!
//! This is the one thing in komo that mounts tools into a *running* process
//! rather than at wiring, so it is where the catalog's runtime half earns its
//! keep. Writing a plugin file makes a tool appear on the next turn; deleting
//! it makes the tool go away; a host that crashes takes its tools with it and
//! brings them back when it restarts.
//!
//! The protocol and the child process live in `komo-pyhost`; the adapter that
//! makes a plugin's function look like a [`Tool`] lives in `komo-tools`. What
//! lives here is the lifecycle: when to spawn, what to mount it into, and what
//! to do when it dies.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use komo_core::domain::catalog::{Registration, ToolCatalog};
use komo_core::domain::policy::{Category, Policy};
use komo_core::domain::tool::Tool;
use komo_pyhost::{HostEvent, PluginToolDef, PyHost, SharedHost};
use komo_tools::plugin::PyTool;

/// The interpreter the host runs on.
///
/// Not configurable: every platform komo targets ships one under this name, and
/// a plugin needing a specific environment is better served by installing its
/// dependencies into that interpreter than by komo learning to pick one.
const PYTHON: &str = "python3";

/// How long to wait before restarting a host that exited, and the ceiling that
/// backoff climbs to.
///
/// A host that dies on startup (a syntax error in the embedded runtime, an
/// interpreter that vanished) would otherwise respawn in a tight loop; a host
/// that died once should come back promptly, because the tools are gone until
/// it does.
const RESTART_DELAY: Duration = Duration::from_secs(2);
const RESTART_DELAY_MAX: Duration = Duration::from_secs(60);

/// Start the plugin host and keep it mounted in `catalogs` for the life of the
/// process. `None` = no host (opted out, no interpreter, or a policy that
/// denies plugins outright), which costs the plugin tools and `run_code` and
/// nothing else.
pub fn start(
    home: &std::path::Path,
    enabled: bool,
    policy: &Policy,
    catalogs: Vec<Arc<ToolCatalog>>,
) -> Option<SharedHost> {
    if !enabled {
        tracing::info!("the python plugin host is disabled (`pyhost_enabled = false`)");
        return None;
    }
    // A policy that denies plugins outright is answered by not starting the
    // host at all: the tools would be dropped from every catalog anyway, and an
    // interpreter running plugin code nobody may call is worse than pointless.
    if policy.wholly_denied(Category::Plugin, None) {
        tracing::info!(
            "[policy] denies the `plugin` category; the python plugin host is not started"
        );
        return None;
    }
    let plugins_dir = home.join("plugins");
    // Created rather than waited for. The directory used to be the opt-in, on
    // the theory that an interpreter watching a directory nobody asked for is
    // waste — but run_code rides the same host, and code mode is part of the
    // default toolset, so the host earns its keep with zero plugin files. The
    // old gate also failed silently: two deployments ran without run_code for
    // days because nobody knew a mkdir was the switch.
    if let Err(error) = std::fs::create_dir_all(&plugins_dir) {
        tracing::warn!(
            %error,
            dir = %plugins_dir.display(),
            "could not create the plugin directory; the python plugin host is not started"
        );
        return None;
    }
    // One probe before committing to a supervisor: without an interpreter the
    // restart loop would warn every minute forever on a machine that is simply
    // never going to have one. Absence is a clean, one-line skip.
    if std::process::Command::new(PYTHON)
        .arg("--version")
        .output()
        .is_err()
    {
        tracing::warn!(
            "`{PYTHON}` not found; the python plugin host is not started \
             (install python3, or set `pyhost_enabled = false` to silence this)"
        );
        return None;
    }
    // The slot the supervisor keeps current across restarts. `run_code` holds
    // it rather than a host handle, so a restarted host is picked up without
    // re-registering the tool.
    let host = SharedHost::default();
    let supervisor = Supervisor {
        home: home.to_path_buf(),
        plugins_dir,
        catalogs,
        host: host.clone(),
    };
    // Supervised in the background: a plugin host that will not start must cost
    // the plugins, never the boot. Its first attempt is made here rather than
    // deferred, so the usual case (a working host) has its tools mounted before
    // the first turn.
    tokio::spawn(supervisor.run());
    Some(host)
}

/// Owns one plugin host across restarts, and the registrations that keep its
/// tools mounted.
struct Supervisor {
    home: PathBuf,
    plugins_dir: PathBuf,
    catalogs: Vec<Arc<ToolCatalog>>,
    /// Published so `run_code` can reach whichever host is current.
    host: SharedHost,
}

impl Supervisor {
    /// Run until the process ends: spawn, mount, follow the host's events, and
    /// respawn when it dies.
    async fn run(self) {
        let mut delay = RESTART_DELAY;
        loop {
            match self.serve_one_host().await {
                // The host exited. Its registrations dropped with the loop
                // body, so its tools are already out of every catalog — the
                // model will not be offered a tool nothing can answer.
                Ok(status) => {
                    tracing::warn!(
                        status = %status,
                        delay_secs = delay.as_secs(),
                        "python plugin host exited; restarting"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        error = format!("{error:#}"),
                        delay_secs = delay.as_secs(),
                        "python plugin host unavailable; retrying"
                    );
                }
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(RESTART_DELAY_MAX);
        }
    }

    /// One host's whole life: spawn it, mount what it reports, re-mount on
    /// every change, and return when it goes away.
    async fn serve_one_host(&self) -> anyhow::Result<String> {
        let (host, mut events) = PyHost::spawn(PYTHON, &self.home, &self.plugins_dir).await?;
        let manifest = host.manifest().await?;
        // Published only once the host answered: a handle to a process that
        // cannot speak the protocol is worse than none, because `run_code`
        // would hand programs to it and wait.
        self.host.set(Some(host.clone()));

        // Held across the loop: dropping these unmounts, which is exactly what
        // should happen when this function returns for any reason.
        let mut mounted = self.mount(&host, manifest.tools);

        while let Some(event) = events.recv().await {
            match event {
                HostEvent::ManifestChanged(manifest) => {
                    // Replace wholesale rather than diffing: the host reports
                    // the complete set, and a batch mount is one change to the
                    // model's view — so one prompt-cache invalidation, whether
                    // one tool changed or ten.
                    drop(std::mem::take(&mut mounted));
                    mounted = self.mount(&host, manifest.tools);
                }
                HostEvent::Exited { status } => return Ok(self.retire(status)),
            }
        }
        Ok(self.retire("event stream closed".to_string()))
    }

    /// Stop advertising a host that is gone, so `run_code` says "not running"
    /// instead of handing a program to a dead process.
    fn retire(&self, status: String) -> String {
        self.host.set(None);
        status
    }

    /// Mount `tools` into every catalog this supervisor covers.
    fn mount(&self, host: &PyHost, tools: Vec<PluginToolDef>) -> Vec<Registration> {
        if tools.is_empty() {
            tracing::info!("python plugin host ready; no plugins registered a tool");
            return Vec::new();
        }
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        tracing::info!(
            count = tools.len(),
            tools = %names.join(", "),
            "mounted python plugin tools"
        );
        self.catalogs
            .iter()
            .map(|catalog| {
                // Built per catalog: each `PyTool` leaks its name, but they are
                // the same handful of strings and the alternative is sharing
                // one `Arc<dyn Tool>` across catalogs whose lifetimes differ.
                let adapted: Vec<Arc<dyn Tool>> = tools
                    .iter()
                    .map(|def| Arc::new(PyTool::new(host.clone(), def.clone())) as Arc<dyn Tool>)
                    .collect();
                catalog.mount_all(adapted)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_core::domain::policy::{Effect, Matcher, Rule, Verdict};

    fn deny_plugins() -> Policy {
        Policy::new(
            vec![Rule {
                channels: None,
                category: Category::Plugin,
                matcher: Matcher::Any,
                value: String::new(),
                access: None,
                effect: Effect::Deny,
                include_dangerous: false,
                unattended: false,
            }],
            Verdict::Ask,
        )
    }

    /// An operator who denied the category gets no interpreter at all — the
    /// tools would be dropped from every catalog anyway, so running plugin code
    /// nobody may call is strictly worse than not running it.
    #[test]
    fn a_wholly_denied_policy_stops_the_host_from_starting() {
        let home = std::env::temp_dir().join("komo-pyhost-denied");
        assert!(start(&home, true, &deny_plugins(), Vec::new()).is_none());
    }

    /// The opt-out is checked before anything is created or probed.
    #[test]
    fn disabling_the_host_skips_it_entirely() {
        let home = std::env::temp_dir().join("komo-pyhost-off");
        assert!(start(&home, false, &Policy::default(), Vec::new()).is_none());
        assert!(!home.join("plugins").exists());
    }
}
