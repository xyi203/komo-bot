//! OS-level supervisor for the gateway.
//!
//! macOS uses `launchd`: `komo gateway start` writes a LaunchAgent plist and
//! bootstraps it; launchd then owns the process (`KeepAlive` relaunches it
//! after a crash, `RunAtLoad` starts it at login).
//!
//! Linux uses `systemd --user`: the same command writes a user unit under
//! `~/.config/systemd/user` and `enable --now`s it; systemd then owns the
//! process (`Restart=always` after a crash, `WantedBy=default.target` at
//! login — `loginctl enable-linger $USER` keeps it up while logged out).
//!
//! Other platforms, and Linux without a systemd user session (Docker), should
//! run `komo gateway` in the foreground and let the outer supervisor own
//! start/stop/restart.

/// Install the gateway under the OS supervisor and start it.
pub fn start() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchd::start()
    }
    #[cfg(target_os = "linux")]
    {
        systemd::start()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        unsupported("start")
    }
}

/// Stop the supervised gateway and remove it from the supervisor.
pub fn stop() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchd::stop()
    }
    #[cfg(target_os = "linux")]
    {
        systemd::stop()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        unsupported("stop")
    }
}

/// Stop (if running) and start again — picks up a rebuilt/reinstalled binary.
pub fn restart() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchd::restart()
    }
    #[cfg(target_os = "linux")]
    {
        systemd::restart()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        unsupported("restart")
    }
}

/// Report the supervisor's state for the gateway.
pub fn status() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchd::status()
    }
    #[cfg(target_os = "linux")]
    {
        systemd::status()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        unsupported("status")
    }
}

/// Whether a supervised gateway is currently live. `komo upgrade` uses this to
/// decide whether to restart — so an upgrade never *installs* the supervisor for
/// someone who only runs the gateway in the foreground.
pub fn gateway_loaded() -> anyhow::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let domain = launchd::gui_domain()?;
        Ok(launchd::is_loaded(&domain))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(systemd::is_loaded())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Ok(false)
    }
}

/// The `PATH` a supervised gateway runs with — and so every process the agent's
/// `shell` tool spawns from there.
///
/// Neither supervisor hands a service the environment of the shell that
/// installed it. A systemd user unit starts with the *manager's* `PATH`
/// (`/usr/local/bin:/usr/bin` on a stock setup); launchd's default is narrower
/// still. Inherited unchanged, that is the whole reason a `shell` call inside
/// the gateway cannot find `git`, `cargo` or `komo`: the gateway runs in a
/// smaller world than the terminal that started it.
///
/// So the unit records the `PATH` the installing process had (the operator's own
/// preference, in their own order), the directory this binary itself was run
/// from — the one place komo knows holds a `komo` that works — and a floor of
/// standard locations, for a host whose `PATH` arrived empty. Additions only
/// fill gaps: nothing already inherited is reordered or dropped.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
fn service_path(exe: &std::path::Path) -> String {
    compose_service_path(std::env::var("PATH").ok().as_deref(), exe)
}

/// The pure half of [`service_path`], so the policy is testable without
/// mutating this process's own environment.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
fn compose_service_path(inherited: Option<&str>, exe: &std::path::Path) -> String {
    let mut entries: Vec<String> = Vec::new();
    for entry in inherited.unwrap_or_default().split(':') {
        push_path_entry(&mut entries, entry);
    }
    if let Some(dir) = exe.parent() {
        push_path_entry(&mut entries, &dir.to_string_lossy());
    }
    for entry in FALLBACK_PATH {
        push_path_entry(&mut entries, entry);
    }
    entries.join(":")
}

/// What the gateway gets whether or not the installing shell had it: the
/// standard system locations, so `sh` and its usual neighbours resolve even
/// from an empty `PATH`. Deliberately not a guess at the user's own layout
/// (`~/.cargo/bin`, mise shims, …) — those are on the inherited `PATH` of
/// whoever ran `komo gateway start`, and inventing a toolchain here would have
/// komo claim paths that may not exist.
const FALLBACK_PATH: &[&str] = &["/usr/local/bin", "/usr/bin", "/bin"];

fn push_path_entry(entries: &mut Vec<String>, entry: &str) {
    let entry = entry.trim();
    if !entry.is_empty() && !entries.iter().any(|seen| seen == entry) {
        entries.push(entry.to_string());
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn the_inherited_path_is_kept_and_only_gaps_are_filled() {
        let path = compose_service_path(
            Some("/usr/local/bin:/home/me/.cargo/bin"),
            std::path::Path::new("/home/me/.local/bin/komo"),
        );
        assert_eq!(
            path, "/usr/local/bin:/home/me/.cargo/bin:/home/me/.local/bin:/usr/bin:/bin",
            "inherited order first, then the binary's own directory, then the floor"
        );
    }

    #[test]
    fn an_empty_inherited_path_still_gets_a_usable_one() {
        // `komo gateway start` from a desktop launcher, or any environment with
        // no PATH at all: the gateway must not end up with a `PATH` of nothing.
        for inherited in [None, Some("")] {
            let path = compose_service_path(inherited, std::path::Path::new("/srv/komo"));
            assert_eq!(path, "/srv:/usr/local/bin:/usr/bin:/bin");
        }
    }

    #[test]
    fn duplicates_are_dropped_and_the_order_is_preserved() {
        let path = compose_service_path(
            Some("/usr/bin:/usr/local/bin:/usr/bin"),
            std::path::Path::new("/srv/bin/komo"),
        );
        assert_eq!(path, "/usr/bin:/usr/local/bin:/srv/bin:/bin");
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn unsupported(action: &str) -> anyhow::Result<()> {
    anyhow::bail!(
        "gateway {action} is supported on macOS (launchd) and Linux (systemd --user) only. \
         Run `komo gateway` in the foreground and let your supervisor own it."
    )
}

// ---------------------------------------------------------------------------
// macOS: launchd LaunchAgent
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod launchd {
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    const LABEL: &str = "com.komo.gateway";
    const BUNDLE_INFO: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/resources/macos/Info.plist"
    ));
    const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

    /// Render the LaunchAgent plist. Pure so the XML is unit-testable.
    /// `exe` is the absolute komo binary path; `log_dir` holds stdout/stderr logs;
    /// `work_dir` is the process working directory (launchd defaults to `/`, which
    /// would make the workspace-confined tools useless); `path` is the `PATH` the
    /// gateway runs with — see [`super::service_path`] for why the supervisor
    /// cannot be left to pick one.
    fn render_plist(exe: &str, log_dir: &str, work_dir: &str, path: &str) -> String {
        let exe = xml_escape(exe);
        let log_dir = xml_escape(log_dir);
        let work_dir = xml_escape(work_dir);
        let path = xml_escape(path);
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>AssociatedBundleIdentifiers</key>
    <array>
        <string>{LABEL}</string>
    </array>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>gateway</string>
    </array>
    <key>WorkingDirectory</key>
    <string>{work_dir}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>StandardOutPath</key>
    <string>{log_dir}/gateway.log</string>
    <key>StandardErrorPath</key>
    <string>{log_dir}/gateway.err.log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path}</string>
    </dict>
</dict>
</plist>
"#
        )
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    fn plist_path_for(label: &str) -> anyhow::Result<PathBuf> {
        let home =
            dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
        Ok(home
            .join("Library/LaunchAgents")
            .join(format!("{label}.plist")))
    }

    fn plist_path() -> anyhow::Result<PathBuf> {
        plist_path_for(LABEL)
    }

    fn gateway_app_path_for(home: &Path) -> PathBuf {
        home.join("Applications").join("Komo Gateway.app")
    }

    fn gateway_exe_path(app: &Path) -> PathBuf {
        app.join("Contents").join("MacOS").join("komo-gateway")
    }

    fn has_gateway_identity(app: &Path) -> bool {
        Command::new("/usr/bin/codesign")
            .args(["-d", "--verbose=2"])
            .arg(app)
            .output()
            .map(|out| {
                let details = String::from_utf8_lossy(&out.stderr);
                out.status.success()
                    && details.contains("Identifier=com.komo.gateway")
                    && details.contains("Info.plist entries=")
            })
            .unwrap_or(false)
    }

    fn remove_bundle(path: &Path) -> std::io::Result<()> {
        match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn install_gateway_app(source: &Path, destination: &Path) -> anyhow::Result<()> {
        let parent = destination
            .parent()
            .ok_or_else(|| anyhow::anyhow!("gateway app path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let metadata = std::fs::symlink_metadata(parent)?;
        let uid = String::from_utf8_lossy(&Command::new("/usr/bin/id").arg("-u").output()?.stdout)
            .trim()
            .parse::<u32>()?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != uid
            || metadata.mode() & 0o022 != 0
        {
            anyhow::bail!(
                "gateway app directory must be owned by the current user and not group/other-writable: {}",
                parent.display()
            );
        }

        let staging = parent.join(format!(".Komo Gateway.{}.tmp.app", uuid::Uuid::now_v7()));
        let result = (|| -> anyhow::Result<()> {
            let contents = staging.join("Contents");
            let macos = contents.join("MacOS");
            std::fs::create_dir_all(&macos)?;
            std::fs::copy(source, macos.join("komo-gateway"))?;
            std::fs::write(contents.join("Info.plist"), BUNDLE_INFO)?;

            let out = Command::new("/usr/bin/codesign")
                .args(["--force", "--sign", "-"])
                .arg(&staging)
                .output()?;
            if !out.status.success() {
                anyhow::bail!(
                    "failed to sign gateway app: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }

            if !has_gateway_identity(&staging) {
                anyhow::bail!("signed gateway app has no com.komo.gateway identity");
            }

            remove_bundle(destination)?;
            std::fs::rename(&staging, destination)?;

            let out = Command::new(LSREGISTER)
                .arg("-f")
                .arg(destination)
                .output()?;
            if !out.status.success() {
                anyhow::bail!(
                    "failed to register gateway app: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(())
        })();

        if result.is_err() {
            let _ = remove_bundle(&staging);
        }
        result
    }

    /// `gui/<uid>` launchd domain for the current user.
    pub(super) fn gui_domain() -> anyhow::Result<String> {
        let out = Command::new("id").arg("-u").output()?;
        let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if uid.is_empty() {
            anyhow::bail!("could not determine uid via `id -u`");
        }
        Ok(format!("gui/{uid}"))
    }

    fn launchctl(args: &[&str]) -> anyhow::Result<std::process::Output> {
        Command::new("launchctl")
            .args(args)
            .output()
            .map_err(|e| anyhow::anyhow!("failed to run launchctl: {e}"))
    }

    fn is_label_loaded(domain: &str, label: &str) -> bool {
        launchctl(&["print", &format!("{domain}/{label}")])
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    pub(super) fn is_loaded(domain: &str) -> bool {
        is_label_loaded(domain, LABEL)
    }

    /// Poll until launchd has fully unloaded the service, returning whether it did
    /// within the timeout. `bootout` returns before launchd reaps the job, so a
    /// follow-up `start` (which guards on `is_loaded`) would otherwise see it still
    /// present and skip bootstrapping — the restart race.
    fn wait_until_unloaded(domain: &str, label: &str) -> bool {
        // ~5s budget: launchd usually unloads within a few hundred ms.
        for _ in 0..50 {
            if !is_label_loaded(domain, label) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        !is_label_loaded(domain, label)
    }

    fn unload(domain: &str, label: &str) -> anyhow::Result<bool> {
        if !is_label_loaded(domain, label) {
            return Ok(false);
        }
        let out = launchctl(&["bootout", &format!("{domain}/{label}")])?;
        if !out.status.success() {
            anyhow::bail!(
                "launchctl bootout failed for {label}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        if !wait_until_unloaded(domain, label) {
            anyhow::bail!("gateway {label} did not unload after bootout");
        }
        if let Ok(path) = plist_path_for(label) {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(error = %e, label, "could not remove launchd plist"),
            }
        }
        Ok(true)
    }

    /// Write the plist and bootstrap it into the user's gui domain.
    pub fn start() -> anyhow::Result<()> {
        let domain = gui_domain()?;
        if is_label_loaded(&domain, LABEL) {
            tracing::info!(
                "komo gateway is already running under launchd. Use `komo gateway restart` to restart it."
            );
            return Ok(());
        }

        let exe = std::env::current_exe()?;
        let home =
            dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
        let komo_home = komo_config::ensure_komo_home();
        let log_dir = komo_home.join("logs");
        std::fs::create_dir_all(&log_dir)?;
        let gateway_app = gateway_app_path_for(&home);
        let gateway_exe = gateway_exe_path(&gateway_app);
        install_gateway_app(&exe, &gateway_app)?;

        let path = plist_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &path,
            render_plist(
                &gateway_exe.display().to_string(),
                &log_dir.display().to_string(),
                &komo_home.display().to_string(),
                &super::service_path(&exe),
            ),
        )?;

        let out = launchctl(&["bootstrap", &domain, &path.display().to_string()])?;
        if !out.status.success() {
            anyhow::bail!(
                "launchctl bootstrap failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        tracing::info!(
            "komo gateway started under launchd ({LABEL}); it restarts on crash and at login. \
             Logs: {}/gateway.log",
            log_dir.display()
        );
        Ok(())
    }

    /// Remove the service from launchd (stops the process and disables auto-restart).
    pub fn stop() -> anyhow::Result<()> {
        let domain = gui_domain()?;
        if !unload(&domain, LABEL)? {
            println!("komo gateway is not running under launchd.");
            return Ok(());
        }
        println!("komo gateway stopped.");
        Ok(())
    }

    /// Stop (if loaded), regenerate the plist, and start again. Regenerating means
    /// a rebuilt/reinstalled binary or moved log dir is picked up on restart.
    pub fn restart() -> anyhow::Result<()> {
        let domain = gui_domain()?;
        unload(&domain, LABEL)?;
        start()
    }

    /// Report whether launchd has the service and whether the process is running.
    pub fn status() -> anyhow::Result<()> {
        let domain = gui_domain()?;
        let out = launchctl(&["print", &format!("{domain}/{LABEL}")])?;
        if !out.status.success() {
            println!("komo gateway: not loaded (run `komo gateway start`).");
            return Ok(());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        // Surface just the interesting lines from launchctl's verbose dump.
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("state =")
                || trimmed.starts_with("pid =")
                || trimmed.starts_with("path =")
                || trimmed.starts_with("last exit code =")
            {
                println!("{trimmed}");
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn plist_contains_label_exe_keepalive_workdir_and_path() {
            let plist = render_plist(
                "/usr/local/bin/komo",
                "/Users/me/.komo/logs",
                "/Users/me/.komo",
                "/usr/local/bin:/usr/bin",
            );
            assert!(plist.contains("<string>com.komo.gateway</string>"));
            assert!(plist.contains("<key>AssociatedBundleIdentifiers</key>"));
            assert!(plist.contains("<string>/usr/local/bin/komo</string>"));
            assert!(plist.contains("<string>gateway</string>"));
            assert!(plist.contains("<key>KeepAlive</key>"));
            assert!(plist.contains("/Users/me/.komo/logs/gateway.log"));
            assert!(plist.contains("<key>WorkingDirectory</key>"));
            assert!(plist.contains("<string>/Users/me/.komo</string>"));
            // launchd's own default is `/usr/bin:/bin:/usr/sbin:/sbin`: without
            // this key the gateway's `shell` calls run without git.
            assert!(plist.contains("<key>EnvironmentVariables</key>"));
            assert!(plist.contains("<string>/usr/local/bin:/usr/bin</string>"));
        }

        #[test]
        fn plist_escapes_xml_special_chars_in_paths() {
            let plist = render_plist("/odd<&>path/komo", "/logs", "/work", "/a<b>&c");
            assert!(plist.contains("/a&lt;b&gt;&amp;c"));
            assert!(plist.contains("/odd&lt;&amp;&gt;path/komo"));
            assert!(!plist.contains("/odd<&>path"));
        }

        #[test]
        fn managed_gateway_executable_lives_in_an_app_bundle() {
            let app = gateway_app_path_for(Path::new("/Users/me"));
            assert_eq!(
                gateway_exe_path(&app),
                PathBuf::from(
                    "/Users/me/Applications/Komo Gateway.app/Contents/MacOS/komo-gateway"
                )
            );
        }

        #[test]
        fn gateway_bundle_declares_its_identity_and_local_network_usage() {
            let plist = String::from_utf8_lossy(BUNDLE_INFO);
            assert!(plist.contains("<string>com.komo.gateway</string>"));
            assert!(plist.contains("<key>CFBundleExecutable</key>"));
            assert!(plist.contains("<key>NSLocalNetworkUsageDescription</key>"));
        }
    }
}

// ---------------------------------------------------------------------------
// Linux: systemd user unit
// ---------------------------------------------------------------------------

/// The unit file itself — pure, so it is unit-testable on any platform.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod systemd_unit {
    pub(super) const UNIT: &str = "komo-gateway.service";

    /// Render the systemd user unit. `exe` is the absolute komo binary path,
    /// `komo_home` the resolved komo home (working directory), `log_dir` holds
    /// the stdout/stderr capture — named as in the launchd plist so `komo logs`
    /// falls back to the same files.
    ///
    /// `Environment=KOMO_HOME` pins the supervised gateway to the home this CLI
    /// resolved while installing the unit: without it a `KOMO_HOME` exported
    /// only in the installing shell would leave the two disagreeing.
    ///
    /// `Environment=PATH` is the same idea one step further out: a user unit
    /// starts with the *manager's* `PATH`, never the installing shell's, so
    /// without this line every `shell` call the agent makes runs without `git`,
    /// `cargo` or `komo` — see [`super::service_path`].
    pub(super) fn render_unit(exe: &str, komo_home: &str, log_dir: &str, path: &str) -> String {
        let exe_q = quote(exe);
        let home_q = quote(komo_home);
        let path_q = quote(path);
        format!(
            "[Unit]
Description=komo gateway

[Service]
ExecStart=\"{exe_q}\" gateway
WorkingDirectory={komo_home}
Environment=\"KOMO_HOME={home_q}\"
Environment=\"PATH={path_q}\"
Restart=always
RestartSec=10
StandardOutput=append:{log_dir}/gateway.log
StandardError=append:{log_dir}/gateway.err.log

[Install]
WantedBy=default.target
"
        )
    }

    /// systemd's double-quoted values escape with a backslash.
    fn quote(s: &str) -> String {
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn unit_contains_exec_start_workdir_home_path_restart_and_logs() {
            let unit = render_unit(
                "/usr/local/bin/komo",
                "/home/me/.komo",
                "/home/me/.komo/logs",
                "/home/me/.cargo/bin:/usr/local/bin:/usr/bin",
            );
            assert!(unit.contains("ExecStart=\"/usr/local/bin/komo\" gateway"));
            assert!(unit.contains("WorkingDirectory=/home/me/.komo"));
            assert!(unit.contains("Environment=\"KOMO_HOME=/home/me/.komo\""));
            assert!(
                unit.contains("Environment=\"PATH=/home/me/.cargo/bin:/usr/local/bin:/usr/bin\"")
            );
            assert!(unit.contains("Restart=always"));
            assert!(unit.contains("StandardOutput=append:/home/me/.komo/logs/gateway.log"));
            assert!(unit.contains("StandardError=append:/home/me/.komo/logs/gateway.err.log"));
            assert!(unit.contains("WantedBy=default.target"));
        }

        #[test]
        fn unit_escapes_quotes_and_a_literal_percent_in_paths() {
            let unit = render_unit("/odd\"path/komo", "/home", "/logs", "/odd%path");
            assert!(
                unit.contains("Environment=\"PATH=/odd%%path\""),
                "systemd expands `%` even in Environment=, so a literal one is doubled: {unit}"
            );
            assert!(unit.contains("ExecStart=\"/odd\\\"path/komo\" gateway"));
        }
    }
}

#[cfg(target_os = "linux")]
mod systemd {
    use std::io::ErrorKind;
    use std::path::PathBuf;
    use std::process::{Command, Output};

    use super::systemd_unit::{UNIT, render_unit};

    fn unit_path() -> anyhow::Result<PathBuf> {
        let config = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot determine config directory"))?;
        Ok(config.join("systemd").join("user").join(UNIT))
    }

    fn systemctl(args: &[&str]) -> anyhow::Result<Output> {
        Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .map_err(|e| {
                if e.kind() == ErrorKind::NotFound {
                    anyhow::anyhow!(
                        "systemd is not available here (no `systemctl`). Run `komo gateway` in \
                         the foreground and let your supervisor own it — in Docker that is the \
                         container's main process."
                    )
                } else {
                    anyhow::anyhow!("failed to run systemctl: {e}")
                }
            })
    }

    fn check(out: &Output, command: &str) -> anyhow::Result<()> {
        if out.status.success() {
            return Ok(());
        }
        let raw = String::from_utf8_lossy(&out.stderr);
        let stderr = raw.trim();
        let hint = if stderr.contains("Failed to connect to bus") {
            " (no systemd user session — log in on the machine, or use `loginctl enable-linger $USER`)"
        } else {
            ""
        };
        anyhow::bail!("systemctl {command} failed: {stderr}{hint}")
    }

    fn load_state() -> anyhow::Result<String> {
        let out = systemctl(&["show", "-p", "LoadState", "--value", UNIT])?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    pub(super) fn is_loaded() -> bool {
        load_state().map(|state| state == "loaded").unwrap_or(false)
    }

    /// A missing `systemctl` propagates, so nothing below writes a unit file
    /// on a host that has no systemd to read it.
    fn is_active() -> anyhow::Result<bool> {
        Ok(systemctl(&["is-active", "--quiet", UNIT])?.status.success())
    }

    fn unload() -> anyhow::Result<bool> {
        if load_state()? != "loaded" {
            return Ok(false);
        }
        check(&systemctl(&["disable", "--now", UNIT])?, "disable --now")?;
        if let Ok(path) = unit_path() {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(error = %e, unit = UNIT, "could not remove systemd unit"),
            }
        }
        check(&systemctl(&["daemon-reload"])?, "daemon-reload")?;
        Ok(true)
    }

    /// Write the user unit and `enable --now` it.
    pub fn start() -> anyhow::Result<()> {
        if is_active()? {
            tracing::info!(
                "komo gateway is already running under systemd. Use `komo gateway restart` to restart it."
            );
            return Ok(());
        }

        let exe = std::env::current_exe()?;
        let komo_home = komo_config::ensure_komo_home();
        let log_dir = komo_home.join("logs");
        std::fs::create_dir_all(&log_dir)?;

        let path = unit_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &path,
            render_unit(
                &exe.display().to_string(),
                &komo_home.display().to_string(),
                &log_dir.display().to_string(),
                &super::service_path(&exe),
            ),
        )?;

        check(&systemctl(&["daemon-reload"])?, "daemon-reload")?;
        check(&systemctl(&["enable", "--now", UNIT])?, "enable --now")?;

        tracing::info!(
            "komo gateway started under systemd ({UNIT}); it restarts on crash and at login. \
             To keep it running while logged out, run `loginctl enable-linger $USER`. \
             Logs: {}/gateway.log",
            log_dir.display()
        );
        Ok(())
    }

    /// Disable the unit (stops the process and disables auto-restart) and remove it.
    pub fn stop() -> anyhow::Result<()> {
        if !unload()? {
            println!("komo gateway is not running under systemd.");
            return Ok(());
        }
        println!("komo gateway stopped.");
        Ok(())
    }

    /// Stop (if loaded), regenerate the unit, and start again. Regenerating means
    /// a rebuilt/reinstalled binary or moved log dir is picked up on restart.
    pub fn restart() -> anyhow::Result<()> {
        unload()?;
        start()
    }

    /// Report whether systemd has the unit and what the process is doing.
    pub fn status() -> anyhow::Result<()> {
        let out = systemctl(&[
            "show",
            UNIT,
            "-p",
            "LoadState,ActiveState,SubState,MainPID,ExecMainStatus,FragmentPath",
        ])?;
        let text = String::from_utf8_lossy(&out.stdout);
        if !text.lines().any(|line| line.trim() == "LoadState=loaded") {
            println!("komo gateway: not loaded (run `komo gateway start`).");
            return Ok(());
        }
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.contains('=') {
                println!("{trimmed}");
            }
        }
        Ok(())
    }
}
