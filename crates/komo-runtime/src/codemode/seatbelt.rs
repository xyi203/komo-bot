//! macOS 的 codemode 沙箱（`docs/codemode.md` §4）：`sandbox-exec` + 按解释器现生成的 Seatbelt 配置。

use std::path::Path;

use super::Paths;

#[derive(Debug, Clone)]
pub(super) struct Jail {
    profile: String,
}

impl Jail {
    pub(super) fn new(paths: &Paths) -> Result<Jail, String> {
        Ok(Jail {
            profile: profile(paths),
        })
    }

    pub(super) fn command(&self, interpreter: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("/usr/bin/sandbox-exec");
        command.arg("-p").arg(&self.profile).arg(interpreter);
        command
    }
}

/// 默认全拒，只放解释器与它的安装目录。
fn profile(paths: &Paths) -> String {
    let quote = |path: &Path| format!("{:?}", path.display().to_string());
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let real = canonical(&paths.executable);
    // seatbelt 按解析后的真实路径匹配：Homebrew 的 framework 版 Python 报的 `sys.prefix` 走
    // `/opt/homebrew/opt/…` 这条符号链接，dyld 却从 `Cellar/…` 加载 `Python.framework`，只放
    // 前者就"Library not loaded"（2026-10-14 实测 3.14）。两份都放，去重后拼成一串。
    let mut roots: Vec<String> = Vec::new();
    for root in [
        paths.prefix.clone(),
        paths.base_prefix.clone(),
        canonical(&paths.prefix),
        canonical(&paths.base_prefix),
    ] {
        let quoted = format!("(subpath {})", quote(&root));
        if !roots.contains(&quoted) {
            roots.push(quoted);
        }
    }
    let roots = roots.join(" ");
    format!(
        r#"(version 1)
(deny default)
(allow process-exec (literal {exe}) (literal {real}) {roots})
(allow process-fork)
(allow file-map-executable)
(allow file-read-metadata)
(allow file-read* {roots} (subpath "/System") (subpath "/usr/lib")
                  (subpath "/private/var/db/dyld") (literal "/dev/null") (literal "/dev/urandom")
                  (literal "/"))
(allow sysctl-read)
(allow file-write-data (literal "/dev/null"))
"#,
        exe = quote(&paths.executable),
        real = quote(&real),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_symlinked_prefix_is_allowed_under_its_real_path_too() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("Cellar/python/3.14");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("opt-python");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let profile = profile(&Paths {
            executable: link.join("bin/python3.14"),
            prefix: link.clone(),
            base_prefix: link.clone(),
        });
        let real = std::fs::canonicalize(&real).unwrap();
        assert!(
            profile.contains(&format!("(subpath {:?})", link.display().to_string())),
            "{profile}"
        );
        assert!(
            profile.contains(&format!("(subpath {:?})", real.display().to_string())),
            "{profile}"
        );
        // prefix 与 base_prefix 相同，不重复。
        assert_eq!(
            profile
                .matches(&format!("(subpath {:?})", real.display().to_string()))
                .count(),
            2
        );
    }
}
