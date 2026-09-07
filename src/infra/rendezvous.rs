//! Gateway rendezvous file (`~/.komo/gateway.json`).
//!
//! Turso takes an exclusive cross-process lock on each db file, so while the
//! gateway runs it is the *sole* owner of `state.db` / `memory.db`. The CLI
//! therefore can't open them directly — it has to route
//! requests to the running gateway over the api channel's loopback HTTP. This
//! file is how the CLI *discovers* that gateway: the gateway writes its bind
//! address, port, bearer key, and pid here on startup, and removes it on
//! graceful shutdown.
//!
//! The file is only a hint — a crashed gateway leaves a stale file behind, so
//! callers always probe `/health` (see the gateway client) rather than trusting
//! the file's mere existence. The bearer key lives here (mode `0600`) so the CLI
//! need not re-derive the auto-generated loopback key from anywhere else.

use std::path::PathBuf;

use komo_config::komo_home;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Where the gateway advertises how to reach its loopback api channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayInfo {
    pub pid: u32,
    pub bind: String,
    pub port: u16,
    pub key: String,
}

impl GatewayInfo {
    /// Base URL the CLI client posts against (e.g. `http://127.0.0.1:8765`).
    /// A `0.0.0.0` bind is reached over loopback.
    pub fn base_url(&self) -> String {
        let host = if self.bind == "0.0.0.0" {
            "127.0.0.1"
        } else {
            &self.bind
        };
        format!("http://{host}:{}", self.port)
    }
}

/// `~/.komo/gateway.json`.
pub fn path() -> PathBuf {
    komo_home().join("gateway.json")
}

/// Record how to reach the running gateway. Best-effort: a failure is logged
/// but never aborts gateway startup (the CLI simply falls back to the db).
pub fn write(info: &GatewayInfo) {
    let p = path();
    let body = match serde_json::to_vec_pretty(info) {
        Ok(body) => body,
        Err(error) => {
            warn!(%error, "failed to serialize gateway rendezvous");
            return;
        }
    };
    if let Err(error) = std::fs::write(&p, body) {
        warn!(%error, path = %p.display(), "failed to write gateway rendezvous");
        return;
    }
    restrict_perms(&p);
}

/// Remove the rendezvous file on shutdown. Best-effort (missing file is fine).
pub fn clear() {
    let p = path();
    if let Err(error) = std::fs::remove_file(&p)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        warn!(%error, path = %p.display(), "failed to clear gateway rendezvous");
    }
}

/// Read the rendezvous file, if present and parseable. `None` means "no gateway
/// advertised" — not an error.
pub fn read() -> Option<GatewayInfo> {
    let body = std::fs::read(path()).ok()?;
    match serde_json::from_slice(&body) {
        Ok(info) => Some(info),
        Err(error) => {
            warn!(%error, "gateway rendezvous file is unparseable; ignoring");
            None
        }
    }
}

#[cfg(unix)]
fn restrict_perms(p: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(error) = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600)) {
        warn!(%error, "failed to chmod gateway rendezvous to 0600");
    }
}

#[cfg(not(unix))]
fn restrict_perms(_p: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_maps_wildcard_bind_to_loopback() {
        let info = GatewayInfo {
            pid: 1,
            bind: "0.0.0.0".into(),
            port: 8765,
            key: "k".into(),
        };
        assert_eq!(info.base_url(), "http://127.0.0.1:8765");
    }

    #[test]
    fn base_url_keeps_explicit_loopback() {
        let info = GatewayInfo {
            pid: 1,
            bind: "127.0.0.1".into(),
            port: 9000,
            key: "k".into(),
        };
        assert_eq!(info.base_url(), "http://127.0.0.1:9000");
    }

    #[test]
    fn round_trips_through_json() {
        let info = GatewayInfo {
            pid: 4242,
            bind: "127.0.0.1".into(),
            port: 8765,
            key: "secret".into(),
        };
        let json = serde_json::to_vec(&info).unwrap();
        let back: GatewayInfo = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.pid, 4242);
        assert_eq!(back.port, 8765);
        assert_eq!(back.key, "secret");
    }
}
