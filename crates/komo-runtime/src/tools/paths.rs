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

/// 一个**已授权根**：和 [`resolve`] 同一种口径的真实路径。
///
/// 根是配置里写的（`workspaces/`）或 Session 的 `workdir`，两者都"没解析过"；而规则
/// 比对的目标路径一定过 [`resolve`]。macOS 上 `/tmp`、`/var` 是指向 `/private/…` 的
/// 符号链接，于是根按字面停在 `/var/folders/…`、目标是 `/private/var/folders/…`，
/// 前缀匹配必然不成立——workspace 里的一次读取会被判成"范围外"去问人。**根也必须
/// 解析符号链接，否则两边的答案不在同一套坐标系里。**
pub fn real_root(path: &Path) -> PathBuf {
    real_path(path).unwrap_or_else(|_| lexical(path))
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

    /// 规则比对的两边必须落在同一套坐标系里：根经符号链接写进来，目标也要能落进它。
    #[test]
    fn a_symlinked_root_still_contains_the_targets_resolved_under_it() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let root = real_root(&link);
        let target = resolve("notes.md", &link).unwrap();
        assert_eq!(root, std::fs::canonicalize(&real).unwrap());
        assert!(
            target.starts_with(&root),
            "{target:?} 落不进根 {root:?}——这就是 workspace 内部读取被判成范围外"
        );
    }

    /// 根还不存在时（Session 的 workdir 刚建、`workspaces/` 还没用）也照样是绝对的真实
    /// 路径：不存在的尾段按字面保留，存在的祖先照样解析。
    #[test]
    fn a_root_that_does_not_exist_yet_keeps_its_resolved_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let root = real_root(&link.join("not-yet").join("deeper"));
        assert_eq!(
            root,
            std::fs::canonicalize(&real)
                .unwrap()
                .join("not-yet")
                .join("deeper")
        );
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
