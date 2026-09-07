//! komo home-directory resolution.
//!
//! `KOMO_HOME` is the one environment variable read outside the resolved
//! config snapshot, because it decides where `config.toml` and `.env`
//! themselves live.

use std::path::PathBuf;

/// Returns the `~/.komo` config directory. Overridable via `KOMO_HOME`.
///
/// Read directly (not via `KomoEnv`): this is the bootstrap variable that
/// decides where `~/.komo/.env` lives, so it must work before dotenvy has
/// loaded that file.
pub fn komo_home() -> PathBuf {
    std::env::var("KOMO_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .expect("cannot determine home directory")
                .join(".komo")
        })
}

/// Ensure `~/.komo/` exists (0700) and return its path.
/// Tightens `.env` inside to 0600 if present.
/// Permission failures are silently ignored (containers, Windows).
///
/// Permissions are only applied when they are actually wrong: the home dir is
/// chmod'd solely on the run that creates it, and `.env` only when its mode
/// differs from 0600. Re-chmod'ing an existing path on every startup rewrites
/// the ACL on filesystems that keep one (ZFS/NFSv4 — a mounted TrueNAS
/// dataset), which would clobber operator-set ACLs on each gateway restart.
pub fn ensure_komo_home() -> PathBuf {
    let home = komo_home();
    let newly_created = !home.exists();
    if let Err(e) = std::fs::create_dir_all(&home) {
        eprintln!("komo: could not create {}: {e}", home.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if newly_created {
            let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700));
        }
        let env_path = home.join(".env");
        if let Ok(meta) = std::fs::metadata(&env_path)
            && meta.permissions().mode() & 0o777 != 0o600
        {
            let _ = std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600));
        }
    }
    home
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn komo_home_respects_env_override() {
        let dir = std::env::temp_dir().join("komo_core_paths_test_home_override");
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: single-threaded test context; we restore immediately.
        unsafe { std::env::set_var("KOMO_HOME", dir.to_str().unwrap()) };
        let home = komo_home();
        unsafe { std::env::remove_var("KOMO_HOME") };
        assert_eq!(home, dir);
    }
}
