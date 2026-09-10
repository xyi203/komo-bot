//! `komo config check|reload` — the one gesture that applies an edited config.
//!
//! Most of komo's settings are read once, at boot: `[mcp.servers.*]` mounts its
//! tools into a catalog that is immutable afterwards, `pyhost_enabled` decides
//! before the executors exist whether `python` is registered, `[wiki] vault`
//! and `[channels.*]` decide what is constructed at all. So "apply my change"
//! is a restart, and the useful thing a command can add is the part a bare
//! `komo gateway restart` skips: **validating first**.
//!
//! That ordering is the whole point. A typo in `config.toml` found by the
//! restart is a gateway that does not come back — discovered when the next
//! message goes unanswered. Found here, it is a line of output while the
//! operator is still looking at the file.

use komo_config::ConfigSnapshot;

use super::service;

/// Re-read and report, changing nothing.
pub fn check(current: &ConfigSnapshot) -> anyhow::Result<()> {
    let home = current.runtime.home.clone();
    println!("config: {}", home.join("config.toml").display());
    println!("env:    {}", home.join(".env").display());

    let candidate = ConfigSnapshot::load();
    match candidate.validate_gateway() {
        Ok(()) => {
            println!("\n  ok — parses, and the gateway would boot on it");
            report_warnings(&candidate);
            println!(
                "\nThe running gateway still uses the config it booted with. \
                 `komo config reload` applies this one."
            );
            Ok(())
        }
        Err(error) => {
            println!("\n  FAILED — {error}");
            println!("\nNothing was changed; the running gateway is unaffected.");
            Err(error)
        }
    }
}

/// Validate, then restart so the new values take effect.
pub fn reload(current: &ConfigSnapshot) -> anyhow::Result<()> {
    let candidate = ConfigSnapshot::load();
    // Refuse before touching the gateway: a restart onto a config that will
    // not parse takes the agent down until someone notices it is gone.
    if let Err(error) = candidate.validate_gateway() {
        println!("config does not validate — not restarting:\n  {error}");
        println!("\n{}", current.runtime.home.join("config.toml").display());
        return Err(error);
    }
    report_warnings(&candidate);
    println!("config validates; restarting the gateway to apply it…");
    service::restart()
}

/// Non-fatal problems (`validate_gateway` only refuses on fatal ones). Worth
/// printing on the way past: a missing model API key boots fine and then fails
/// every call, which reads as a broken agent rather than a config gap.
fn report_warnings(candidate: &ConfigSnapshot) {
    for issue in &candidate.report.issues {
        println!("  warning: {}", issue.message);
    }
}
