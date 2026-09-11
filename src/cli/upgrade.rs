//! `komo upgrade` — pull the latest source, rebuild + reinstall the binary, and
//! restart the supervised gateway so the new build goes live.
//!
//! komo's analog of hermes' `hermes update` (git pull → reinstall → restart),
//! minus hermes' fork-sync / Windows-ZIP / hangup machinery — komo is a
//! single-user macOS/Rust tool, so the flow stays small:
//!
//!   1. `git pull --ff-only` in the repo this binary was built from
//!      (`CARGO_MANIFEST_DIR`, baked in at compile time).
//!   2. `cargo install --path <repo> --force`, reinstalled to the **currently
//!      running** binary's location.
//!   3. `komo gateway restart` — but only if the gateway is actually loaded
//!      under its supervisor, so an upgrade never installs the service
//!      uninvited. Docker deployments should restart the container/process
//!      outside komo after upgrading.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::service;

pub fn run(no_restart: bool) -> anyhow::Result<()> {
    let baked = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // A binary installed from a *linked* git worktree (e.g. a coding-agent
    // session worktree under `.claude/worktrees/`) bakes that worktree's path.
    // Upgrading there would pull a stale topic branch — or fail outright, since
    // such branches have no upstream. Redirect to the repository's main
    // worktree, which is where `main` lives.
    let repo = match main_worktree_of(&baked) {
        Some(main) if main != baked => {
            println!(
                "(built from the linked worktree {} — upgrading from the main \
                 checkout instead)",
                baked.display()
            );
            main
        }
        _ => baked,
    };
    if !repo.exists() {
        anyhow::bail!(
            "source repo {} no longer exists — `komo upgrade` rebuilds from the \
             checkout this binary was built in. Re-clone it (or rebuild from the new \
             location) and `cargo install --path .` to relink.",
            repo.display()
        );
    }
    println!(
        "komo upgrade — current version {}\nrepo: {}",
        env!("CARGO_PKG_VERSION"),
        repo.display()
    );

    // 1. Pull latest source. `--ff-only` never silently rewrites local work: a
    //    dirty tree or diverged history surfaces as a git error rather than a
    //    surprise reset.
    if repo.join(".git").exists() {
        println!("\n→ git pull --ff-only");
        let ok = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["pull", "--ff-only"])
            .status()
            .map_err(|e| anyhow::anyhow!("failed to run git (is it installed?): {e}"))?
            .success();
        if !ok {
            anyhow::bail!(
                "git pull failed (uncommitted changes, diverged history, or no network?) — \
                 resolve it in {} and retry",
                repo.display()
            );
        }
    } else {
        println!("\n(not a git repo — skipping pull, rebuilding from the current checkout)");
    }

    // 2. Rebuild + reinstall. Overwriting the running binary's file is safe
    //    (the live process keeps its inode; the path gets the new bytes), and
    //    pointing `--root` at the running binary's location means the next
    //    gateway restart actually launches the new build.
    println!("\n→ cargo install --path . --force (rebuild + reinstall)");
    let mut cmd = Command::new("cargo");
    cmd.arg("install").arg("--path").arg(&repo).arg("--force");
    if let Some(root) = cargo_root_for_current_exe() {
        println!("  installing into {}/bin", root.display());
        cmd.arg("--root").arg(&root);
    }
    let ok = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run cargo: {e}"))?
        .success();
    if !ok {
        anyhow::bail!(
            "cargo install failed — the gateway was NOT restarted and the old binary is \
             still live, so you are not left in a broken state"
        );
    }

    // 3. Restart the gateway so the new binary goes live — only if it is being
    //    supervised by komo itself. Docker deployments run the gateway in the
    //    foreground and should be restarted by the outer supervisor.
    if no_restart {
        println!("\n--no-restart: skipped. Run `komo gateway restart` to go live.");
    } else if service::gateway_loaded().unwrap_or(false) {
        println!("\n→ restarting gateway");
        service::restart()?;
    } else {
        println!("\ngateway not running under a supervisor — nothing to restart.");
    }

    println!("\n✓ upgrade complete.");
    Ok(())
}

/// The `cargo install --root` that lands the new binary back on the currently
/// running binary's path: if the live binary is `<root>/bin/komo`, return
/// `<root>` (so `cargo install --root <root>` writes `<root>/bin/komo`).
/// Returns `None` — meaning the default cargo bin dir — when the layout isn't
/// the standard `.../bin/<exe>` (e.g. a `cargo run` dev build under `target/`).
fn cargo_root_for_current_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let bin_dir = exe.parent()?;
    if bin_dir.file_name()?.to_str()? != "bin" {
        return None;
    }
    Some(bin_dir.parent()?.to_path_buf())
}

/// The repository's **main** worktree for a checkout at `repo`: a linked
/// worktree's `--git-common-dir` is `<main>/.git`, so the main worktree is its
/// parent (for the main worktree itself this resolves back to `repo`). `None`
/// when `repo` isn't a git checkout git can describe (also bare repos, whose
/// common dir isn't named `.git`) — the caller then keeps the baked path and
/// the existing "not a git repo" handling applies.
fn main_worktree_of(repo: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let common = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    if common.file_name()? != ".git" {
        return None;
    }
    Some(common.parent()?.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `git` is a build requirement already (skill installs shell out to it),
    /// so the test exercises the real resolution: a linked worktree resolves
    /// to the main checkout, and the main checkout resolves to itself.
    #[test]
    fn main_worktree_resolves_linked_worktrees_to_the_main_checkout() {
        let dir = std::env::temp_dir().join("komo_upgrade_worktree_test");
        let _ = std::fs::remove_dir_all(&dir);
        let main = dir.join("repo");
        std::fs::create_dir_all(&main).unwrap();
        let git = |args: &[&str], cwd: &Path| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?} failed");
        };
        git(&["init", "-q"], &main);
        std::fs::write(main.join("x"), "x").unwrap();
        git(&["add", "."], &main);
        git(&["commit", "-qm", "init"], &main);
        let linked = dir.join("linked");
        git(
            &[
                "worktree",
                "add",
                "-q",
                linked.to_str().unwrap(),
                "-b",
                "topic",
            ],
            &main,
        );

        let resolved = main_worktree_of(&linked).expect("linked worktree resolves");
        assert_eq!(
            resolved.canonicalize().unwrap(),
            main.canonicalize().unwrap()
        );
        let same = main_worktree_of(&main).expect("main worktree resolves");
        assert_eq!(same.canonicalize().unwrap(), main.canonicalize().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn main_worktree_is_none_outside_a_repo() {
        let dir = std::env::temp_dir().join("komo_upgrade_nonrepo_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(main_worktree_of(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
