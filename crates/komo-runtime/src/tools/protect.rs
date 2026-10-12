//! komo 自己的状态不许被 `write` / `edit` 改（§7、§8.10）——Policy 删掉之后唯一保留的
//! 硬限制。
//!
//! 名单由 Gateway 装配时给（数据目录下的 `sessions/`、`state.db`、`runtime/`、`.env`），
//! kernel 不读机器路径，所以它是工具的构造参数，不在 `ToolContext` 上。拒绝发生在
//! `prepare`：计划都不成立，`then_run` 的命令自然也不会跑。
//!
//! 一条 shell 命令写到哪里，事前说不清，`prepare` 拦不住；`shell` 改由 bwrap 把
//! [`Protected::existing`] 列出的路径只读挂上（见 `shell.rs` 的 `Sandbox`）。python 不拦。

use std::path::{Path, PathBuf};

use komo_kernel::types::tool::ToolError;

use super::paths::real_root;

/// 受保护的路径。空名单 = 什么都不拦。
#[derive(Debug, Clone, Default)]
pub struct Protected {
    /// 和计划目标同一种口径的真实路径（[`real_root`]）。
    roots: Vec<PathBuf>,
}

impl Protected {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            roots: paths.iter().map(|path| real_root(path)).collect(),
        }
    }

    /// `path` 是计划里的真实路径。落在名单里就拒，理由点名路径。
    pub fn check(&self, path: &Path) -> Result<(), ToolError> {
        match self.roots.iter().find(|root| covers(root, path)) {
            None => Ok(()),
            Some(root) => Err(ToolError::Denied {
                reason: format!(
                    "{} 在 komo 自己的状态里（{}），write / edit 不能改它（§8.10、§7.2）",
                    path.display(),
                    root.display()
                ),
            }),
        }
    }

    /// 名单里**此刻存在**的路径，连同 [`covers`] 认作同一份的兄弟（`state.db-wal` /
    /// `-log` / `-shm` 这类）。只读挂载只能挂已有的东西，所以每条命令起之前现算。
    pub fn existing(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for root in &self.roots {
            if root.exists() {
                out.push(root.clone());
            }
            let (Some(parent), Some(_)) = (root.parent(), root.file_name()) else {
                continue;
            };
            let Ok(entries) = std::fs::read_dir(parent) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path != *root && covers(root, &path) {
                    out.push(path);
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }
}

/// 自身、底下的一切，以及**同目录下以它的名字开头、后面接 `-` 或 `.` 的兄弟**——
/// `state.db` 的 `-wal` / `-shm` / `-journal` 与它是同一份数据库。
fn covers(root: &Path, path: &Path) -> bool {
    if path.starts_with(root) {
        return true;
    }
    let (Some(name), Some(candidate)) = (root.file_name(), path.file_name()) else {
        return false;
    };
    if path.parent() != root.parent() {
        return false;
    }
    let (name, candidate) = (name.to_string_lossy(), candidate.to_string_lossy());
    candidate
        .strip_prefix(name.as_ref())
        .is_some_and(|rest| rest.starts_with('-') || rest.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> (tempfile::TempDir, Protected) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let protected = Protected::new(vec![
            home.join("sessions"),
            home.join("state.db"),
            home.join("runtime"),
            home.join(".env"),
        ]);
        (dir, protected)
    }

    #[test]
    fn the_state_and_everything_under_it_is_refused() {
        let (dir, protected) = home();
        let home = real_root(dir.path());
        for path in [
            home.join("sessions/s-1/events.jsonl"),
            home.join("state.db"),
            home.join("state.db-wal"),
            home.join("state.db-shm"),
            home.join("runtime/gateway.lock"),
            home.join(".env"),
        ] {
            let error = protected.check(&path).unwrap_err();
            assert!(matches!(error, ToolError::Denied { .. }), "{error:?}");
            assert!(
                error.to_string().contains(&path.display().to_string()),
                "{error}"
            );
        }
    }

    #[test]
    fn neighbours_that_only_share_a_prefix_are_not_the_state() {
        let (dir, protected) = home();
        let home = real_root(dir.path());
        for path in [
            home.join("sessionsX/a.txt"),
            home.join("state.dbx"),
            home.join("workspaces/state.db"),
            home.join("config.toml"),
        ] {
            assert!(protected.check(&path).is_ok(), "{}", path.display());
        }
    }

    #[test]
    fn existing_lists_the_database_siblings_but_not_lookalikes() {
        let (dir, protected) = home();
        let home = real_root(dir.path());
        for name in [
            "state.db",
            "state.db-wal",
            "state.db-log",
            "state.dbx",
            ".env",
        ] {
            std::fs::write(home.join(name), "").unwrap();
        }
        std::fs::create_dir(home.join("sessions")).unwrap();
        assert_eq!(
            protected.existing(),
            vec![
                home.join(".env"),
                home.join("sessions"),
                home.join("state.db"),
                home.join("state.db-log"),
                home.join("state.db-wal"),
            ]
        );
    }

    #[test]
    fn an_empty_list_refuses_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Protected::default()
                .check(&dir.path().join("state.db"))
                .is_ok()
        );
    }
}
