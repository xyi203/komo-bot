//! blob 写入顺序（§3.5，pi-durable 的 effect sandwich）。
//!
//! 大正文（工具输出、图片、产物）仍是 Session 目录里的文件，不进数据库。顺序只有一条：
//!
//! ```text
//! 写 <name>.partial → sync_all → rename → sync 目录 ──▶ 然后才 Line::commit(引用它的 entry)
//! ```
//!
//! blob 先于引用，所以"提交里的引用指向不存在的文件"只可能来自人为删除。反过来的崩溃——
//! blob 已发布、entry 没提交——只留下一个没人引用的文件，无害（可由清理任务回收）。
//! 崩溃前只留下 `.partial`，它不是完成的结果，也永远不会被引用。
//!
//! 这里只包一层现有的发布手法（`tool_output.rs` 的 `.partial` → 同步 → 改名 → 同步目录，
//! 与 `session_log::sync_dir`），给"一次写完整个 blob"的场景用一个函数；流式写入的
//! `FileToolOutputStore` 不动。返回的 [`PublishedBlob`] 是"blob 已落盘"的凭据：调用方拿它
//! 组 entry 里的引用，再提交。

use std::path::Path;

use komo_kernel::traits::StoreError;
use komo_kernel::types::{ContentHash, ContentRef};
use tokio::io::AsyncWriteExt;

use crate::session_dir::sync_dir;

/// 已完整落盘（文件与目录项都已同步）的 blob。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedBlob {
    /// 相对于 `root` 的路径，原样写进 entry 的引用。
    pub reference: ContentRef,
}

/// 把 `bytes` 发布为 `root/relative`（`relative` 带目录时按需建出并同步）。
///
/// 已存在同名文件会被原子替换——blob 写完后不再改（§3.5），调用方用不重复的路径
/// （任务 ID + attempt）保证这一点。
pub async fn publish_blob(
    root: &Path,
    relative: &str,
    bytes: &[u8],
) -> Result<PublishedBlob, StoreError> {
    let final_path = root.join(relative);
    let dir = final_path
        .parent()
        .ok_or_else(|| StoreError::Io(format!("{} 没有父目录", final_path.display())))?;
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| StoreError::Io(format!("建立 {} 失败：{e}", dir.display())))?;
    let mut temp_name = final_path
        .file_name()
        .ok_or_else(|| StoreError::Io(format!("{} 没有文件名", final_path.display())))?
        .to_os_string();
    temp_name.push(".partial");
    let temp_path = dir.join(temp_name);

    let mut file = tokio::fs::File::create(&temp_path)
        .await
        .map_err(|e| StoreError::Io(format!("建立 {} 失败：{e}", temp_path.display())))?;
    file.write_all(bytes)
        .await
        .map_err(|e| StoreError::Io(format!("写 {} 失败：{e}", temp_path.display())))?;
    file.flush()
        .await
        .map_err(|e| StoreError::Io(format!("清空 {} 缓冲失败：{e}", temp_path.display())))?;
    file.sync_all()
        .await
        .map_err(|e| StoreError::Io(format!("同步 {} 失败：{e}", temp_path.display())))?;
    drop(file);
    tokio::fs::rename(&temp_path, &final_path)
        .await
        .map_err(|e| StoreError::Io(format!("发布 {} 失败：{e}", final_path.display())))?;
    sync_dir(dir).await?;

    Ok(PublishedBlob {
        reference: ContentRef {
            path: relative.to_string(),
            size: bytes.len() as u64,
            hash: ContentHash::of_bytes(bytes),
            pointer: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_leaves_the_final_file_and_no_partial() {
        let dir = tempfile::tempdir().unwrap();
        let blob = publish_blob(dir.path(), "tool-output/7/1/output.json", b"hello")
            .await
            .unwrap();
        assert_eq!(blob.reference.size, 5);
        assert_eq!(blob.reference.hash, ContentHash::of_bytes(b"hello"));
        let final_path = dir.path().join("tool-output/7/1/output.json");
        assert_eq!(std::fs::read(&final_path).unwrap(), b"hello");
        assert!(
            !dir.path()
                .join("tool-output/7/1/output.json.partial")
                .exists()
        );
    }
}
