//! 路径解析：模型给的那个字符串 → 计划里写的**真实路径**（§7.1）。
//!
//! 规则只看 [`PlanTarget::path`](komo_kernel::types::plan::PlanTarget::path)，所以
//! 这里必须把符号链接解析掉：一条指向 `/etc/shadow` 的链接放在 workspace 里，按字面
//! 前缀匹配是"根内"，按真实路径是"根外"，两个答案里只有后一个是安全的。
//!
//! 文件还不存在时 `canonicalize` 会失败，但写入一个新文件是正常动作，所以退回到
//! "解析最近的已存在祖先，再把剩下的段接回去"——祖先里的链接照样解析掉，只有还不
//! 存在的那几段是字面的，它们也确实还没有链接可言。

use std::path::{Component, Path, PathBuf};

use komo_kernel::types::tool::ToolError;

/// 把模型给的路径按 `cwd` 解析成绝对路径（还没解析符号链接）。
pub fn absolute(raw: &str, cwd: &Path) -> Result<PathBuf, ToolError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ToolError::InvalidArguments {
            message: "path 不能是空串".into(),
        });
    }
    let path = Path::new(raw);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    })
}

/// 真实路径：符号链接解析后的绝对路径。不存在的尾段按字面保留。
pub fn real_path(path: &Path) -> Result<PathBuf, ToolError> {
    match std::fs::canonicalize(path) {
        Ok(real) => Ok(real),
        Err(_) => {
            // 找最近的已存在祖先。
            let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
            let mut cursor = path;
            loop {
                let Some(parent) = cursor.parent() else {
                    // 一路到根都不存在：只好交出词法规范化的结果。
                    return Ok(lexical(path));
                };
                let Some(name) = cursor.file_name() else {
                    // `..` / `.` 结尾，词法规范化后重来一次。
                    let cleaned = lexical(path);
                    return if cleaned == path {
                        Ok(cleaned)
                    } else {
                        real_path(&cleaned)
                    };
                };
                missing.push(name);
                if let Ok(real) = std::fs::canonicalize(parent) {
                    let mut out = real;
                    for segment in missing.iter().rev() {
                        out.push(segment);
                    }
                    return Ok(out);
                }
                cursor = parent;
            }
        }
    }
}

/// 模型给的路径 → 真实路径，一步。
pub fn resolve(raw: &str, cwd: &Path) -> Result<PathBuf, ToolError> {
    real_path(&absolute(raw, cwd)?)
}

/// 纯词法的 `.` / `..` 折叠。只在文件系统什么都答不上来时用。
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_is_resolved_against_the_context_cwd() {
        let cwd = PathBuf::from("/home/u/ws");
        assert_eq!(
            absolute("src/main.rs", &cwd).unwrap(),
            PathBuf::from("/home/u/ws/src/main.rs")
        );
        assert_eq!(
            absolute("/etc/hosts", &cwd).unwrap(),
            PathBuf::from("/etc/hosts")
        );
    }

    #[test]
    fn an_empty_path_is_a_bad_argument_not_the_cwd() {
        assert!(matches!(
            absolute("  ", Path::new("/home/u")),
            Err(ToolError::InvalidArguments { .. })
        ));
    }

    #[test]
    fn a_symlink_resolves_to_what_it_points_at() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.txt");
        std::fs::write(&real, "hi").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let resolved = real_path(&link).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&real).unwrap());
    }

    #[test]
    fn a_file_that_does_not_exist_yet_keeps_its_name_on_a_resolved_parent() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a");
        std::fs::create_dir(&nested).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&nested, &link).unwrap();

        let resolved = real_path(&link.join("new.txt")).unwrap();
        assert_eq!(
            resolved,
            std::fs::canonicalize(&nested).unwrap().join("new.txt")
        );
    }

    #[test]
    fn dot_dot_does_not_survive_resolution() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        let resolved = real_path(&dir.path().join("a/../a/x.txt")).unwrap();
        assert_eq!(
            resolved,
            std::fs::canonicalize(dir.path().join("a"))
                .unwrap()
                .join("x.txt")
        );
    }
}
