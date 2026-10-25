//! Session 目录的路径与落盘小工具（`docs/komo_bot.md` §8.3）。
//!
//! 新存储下 Session 目录只放**内容**：`tool-output/`（工具输出，[`crate::FileToolOutputStore`]）
//! 与 `artifacts/`；事件与提交在 `durable.db` 里。这里是路径解析（**一律相对于该 Session 目录，
//! 禁止越出**）与"建目录 / 同步目录项"这几个落盘动作。

use std::path::{Component, Path, PathBuf};

use komo_kernel::traits::StoreError;
use komo_kernel::types::ids::SessionId;

/// 一个 Session 目录，以及在它内部解析引用的规则。
///
/// **路径一律相对于该 Session 目录解析**，不依赖进程工作目录；解析时禁止越出该目录
/// （§8.3）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPaths {
    root: PathBuf,
}

impl SessionPaths {
    /// `<sessions_root>/<session_id>`。
    pub fn new(sessions_root: impl AsRef<Path>, session: &SessionId) -> Self {
        Self {
            root: sessions_root.as_ref().join(session.as_str()),
        }
    }

    /// 直接指向一个已知的 Session 目录。
    pub fn at(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn tool_output(&self) -> PathBuf {
        self.root.join("tool-output")
    }

    pub fn artifacts(&self) -> PathBuf {
        self.root.join("artifacts")
    }

    /// 把一个 Session 内的相对引用解析成真实路径。
    ///
    /// 绝对路径、`..` 与符号意义上的越界一律拒绝——引用是受控路径，不是任意文件名。
    pub fn resolve(&self, relative: &str) -> Result<PathBuf, StoreError> {
        let candidate = Path::new(relative);
        if candidate.is_absolute() {
            return Err(StoreError::Corrupt(format!(
                "引用不能是绝对路径：{relative}"
            )));
        }
        let mut out = self.root.clone();
        for component in candidate.components() {
            match component {
                Component::Normal(part) => out.push(part),
                Component::CurDir => {}
                _ => {
                    return Err(StoreError::Corrupt(format!(
                        "引用越出了 Session 目录：{relative}"
                    )));
                }
            }
        }
        Ok(out)
    }

    /// 建出 Session 目录本身，并同步父目录。
    pub async fn ensure(&self) -> Result<(), StoreError> {
        create_dir_all_synced(&self.root).await
    }
}

/// 建目录，并同步它的父目录——新建目录项本身也要落盘（§8.3）。
pub async fn create_dir_all_synced(dir: &Path) -> Result<(), StoreError> {
    if tokio::fs::metadata(dir).await.is_ok() {
        return Ok(());
    }
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| StoreError::Io(format!("建立 {} 失败：{e}", dir.display())))?;
    if let Some(parent) = dir.parent() {
        sync_dir(parent).await?;
    }
    Ok(())
}

/// 同步一个目录的目录项。
pub async fn sync_dir(dir: &Path) -> Result<(), StoreError> {
    let file = tokio::fs::File::open(dir)
        .await
        .map_err(|e| StoreError::Io(format!("打开目录 {} 失败：{e}", dir.display())))?;
    file.sync_all()
        .await
        .map_err(|e| StoreError::Io(format!("同步目录 {} 失败：{e}", dir.display())))
}

/// 同步一个文件的内容。
pub(crate) async fn sync_file(path: &Path) -> Result<(), StoreError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| StoreError::Io(format!("打开 {} 失败：{e}", path.display())))?;
    file.sync_all()
        .await
        .map_err(|e| StoreError::Io(format!("同步 {} 失败：{e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_resolve_inside_the_session_directory_and_never_escape() {
        let paths = SessionPaths::new("/data/sessions", &SessionId::from_raw("s1"));
        assert_eq!(
            paths.resolve("tool-output/a/b.txt").unwrap(),
            PathBuf::from("/data/sessions/s1/tool-output/a/b.txt")
        );
        assert!(paths.resolve("../s2/x").is_err());
        assert!(paths.resolve("/etc/passwd").is_err());
    }

    #[tokio::test]
    async fn ensure_creates_the_directory_and_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(root.path(), &SessionId::from_raw("s1"));
        paths.ensure().await.unwrap();
        paths.ensure().await.unwrap();
        assert!(paths.root().is_dir());
    }
}
