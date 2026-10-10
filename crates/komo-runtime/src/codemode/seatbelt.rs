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
    let real =
        std::fs::canonicalize(&paths.executable).unwrap_or_else(|_| paths.executable.clone());
    let prefix = quote(&paths.prefix);
    let base = quote(&paths.base_prefix);
    format!(
        r#"(version 1)
(deny default)
(allow process-exec (literal {exe}) (literal {real}) (subpath {prefix}) (subpath {base}))
(allow process-fork)
(allow file-map-executable)
(allow file-read-metadata)
(allow file-read* (subpath {prefix}) (subpath {base}) (subpath "/System") (subpath "/usr/lib")
                  (subpath "/private/var/db/dyld") (literal "/dev/null") (literal "/dev/urandom")
                  (literal "/"))
(allow sysctl-read)
(allow file-write-data (literal "/dev/null"))
"#,
        exe = quote(&paths.executable),
        real = quote(&real),
    )
}
