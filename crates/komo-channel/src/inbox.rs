//! 渠道收到的图片：存成本地文件，正文里只给模型一个路径（§11.2）。
//!
//! 入站消息只有文本（[`InboundMessage::text`](komo_kernel::protocol::InboundMessage)），
//! 所以图片不进消息结构：渠道把它下载下来、按内容哈希存进
//! `{workspaces_dir}/inbox/{平台}/`，再把"存在哪"写进正文，模型自己用 `read` 去看。代价是
//! 每张图多一轮模型往返；换来的是 Dispatcher、受理、回放、记忆提取一行都不用认识图片。
//!
//! 三条规矩：
//!
//! 1. **说明在前，用户的文字在后**。图片附言如果是 `/pending`，放在前面它就会被当成一条
//!    命令——而用户发的是一张图。
//! 2. **下载失败不丢消息**。正文里写明哪张没拿到、为什么，模型照样能回一句"图没收到"；
//!    静默丢掉的话用户只会看到机器人不理人。
//! 3. **按内容命名**。同一张图重投两次落在同一个文件上，不会越存越多。
//!
//! 放在工作区里而不是数据目录：`read` 与 codemode 都读得到工作区，而 codemode 碰不到
//! komo 自己的状态目录。

use std::path::{Path, PathBuf};

use crate::ChannelContext;
use komo_kernel::types::chat::ChannelPlatform;
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::tool::{image_extension, image_mime};

/// 一张图最多下载多大。Telegram 的 `getFile` 上限就是 20 MB；再大的东西不该从聊天里进来。
pub const DOWNLOAD_LIMIT_BYTES: usize = 20 * 1024 * 1024;

/// 文件名取内容哈希的前多少位。
const NAME_HASH_CHARS: usize = 16;

/// 一个平台的图片收件箱。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInbox {
    dir: PathBuf,
}

impl ImageInbox {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `{workspaces_dir}/inbox/{平台}`。
    pub fn for_platform(snapshot: &ChannelContext, platform: ChannelPlatform) -> Self {
        Self::new(
            snapshot
                .workspaces_dir
                .join("inbox")
                .join(platform.to_string()),
        )
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 存一张图，返回它的绝对路径。已经有同名文件就不再写。
    ///
    /// 认不出格式也照存（扩展名 `bin`）：HEIC 之类模型看不了，但模型可以先用 `sips` 转。
    pub async fn save(&self, bytes: &[u8]) -> Result<PathBuf, String> {
        if bytes.is_empty() {
            return Err("下载到的是空内容".into());
        }
        if bytes.len() > DOWNLOAD_LIMIT_BYTES {
            return Err(format!(
                "图片 {} 字节，超过 {} 字节的上限",
                bytes.len(),
                DOWNLOAD_LIMIT_BYTES
            ));
        }
        let extension = image_mime(bytes).map_or("bin", image_extension);
        let hash = ContentHash::of_bytes(bytes);
        let path = self
            .dir
            .join(format!("{}.{extension}", &hash.as_str()[..NAME_HASH_CHARS]));
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(path);
        }
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|error| format!("建不了目录 {}：{error}", self.dir.display()))?;
        // 先写临时文件再改名：写到一半的文件不该以最终的名字出现。
        let partial = path.with_extension(format!("{extension}.partial"));
        tokio::fs::write(&partial, bytes)
            .await
            .map_err(|error| format!("写不了 {}：{error}", partial.display()))?;
        tokio::fs::rename(&partial, &path)
            .await
            .map_err(|error| format!("改名到 {} 失败：{error}", path.display()))?;
        Ok(path)
    }
}

/// 一张图的结局：存到了哪，或者为什么没拿到。
pub type SavedImage = Result<PathBuf, String>;

/// 拼正文：每张图一行说明，用户自己的文字跟在后面。没有图就原样返回文字。
pub fn compose_text(images: &[SavedImage], text: &str) -> String {
    let mut lines: Vec<String> = images
        .iter()
        .map(|image| match image {
            Ok(path) => format!(
                "[用户发来一张图片，已存到 {}，用 read 查看]",
                path.display()
            ),
            Err(reason) => format!("[用户发来一张图片，但没能下载：{reason}]"),
        })
        .collect();
    let text = text.trim();
    if !text.is_empty() {
        lines.push(text.to_string());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest-of-png";

    #[tokio::test]
    async fn an_image_lands_under_its_content_hash_with_the_right_extension() {
        let dir = tempfile::tempdir().expect("临时目录");
        let inbox = ImageInbox::new(dir.path().join("inbox/wechat"));

        let path = inbox.save(PNG).await.expect("存得下");
        assert_eq!(path.parent(), Some(inbox.dir()));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("png"));
        assert_eq!(std::fs::read(&path).expect("读回"), PNG);

        let again = inbox.save(PNG).await.expect("重投");
        assert_eq!(again, path, "同一张图落在同一个文件上");
        assert_eq!(std::fs::read_dir(inbox.dir()).expect("列目录").count(), 1);
    }

    #[tokio::test]
    async fn an_unknown_format_is_still_kept() {
        let dir = tempfile::tempdir().expect("临时目录");
        let inbox = ImageInbox::new(dir.path());
        let path = inbox.save(b"ftypheic....").await.expect("照存");
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("bin"));
    }

    #[tokio::test]
    async fn empty_or_oversized_content_is_refused() {
        let dir = tempfile::tempdir().expect("临时目录");
        let inbox = ImageInbox::new(dir.path());
        assert!(inbox.save(b"").await.is_err());
        let huge = vec![0u8; DOWNLOAD_LIMIT_BYTES + 1];
        let error = inbox.save(&huge).await.expect_err("超限");
        assert!(error.contains("上限"), "{error}");
    }

    #[test]
    fn the_inbox_lives_in_the_workspace() {
        let mut snapshot = crate::test_context();
        snapshot.workspaces_dir = PathBuf::from("/w");
        assert_eq!(
            ImageInbox::for_platform(&snapshot, ChannelPlatform::Feishu).dir(),
            Path::new("/w/inbox/feishu")
        );
    }

    #[test]
    fn notes_come_before_the_users_own_words() {
        let text = compose_text(
            &[Ok(PathBuf::from("/w/a.png")), Err("HTTP 404".into())],
            "  /pending  ",
        );
        assert_eq!(
            text,
            "[用户发来一张图片，已存到 /w/a.png，用 read 查看]\n\
             [用户发来一张图片，但没能下载：HTTP 404]\n\
             /pending"
        );
        assert!(!text.starts_with('/'), "附言不能被当成命令");
        assert_eq!(compose_text(&[], " 在吗 "), "在吗");
    }
}
