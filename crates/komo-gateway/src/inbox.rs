//! 网页附件：存成本地文件，正文里只给模型一个路径（§11.2，聊天渠道删除后只剩网页这一个来源）。
//!
//! 图片不进消息结构：网页把它上传、按内容哈希存进 `{workspaces_dir}/inbox/web/`，再把"存在哪"
//! 写进正文，模型自己用 `read` 去看。代价是每张图多一轮模型往返；换来的是受理、回放、记忆提取
//! 一行都不用认识图片。
//!
//! 规矩：**按内容命名**。同一张图重传两次落在同一个文件上，不会越存越多。
//!
//! 放在工作区里而不是数据目录：`read` 与 codemode 都读得到工作区，而 codemode 碰不到
//! komo 自己的状态目录。

use std::path::{Path, PathBuf};

use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::tool::{image_extension, image_mime};

/// 一个附件最多多大。
pub const DOWNLOAD_LIMIT_BYTES: usize = 20 * 1024 * 1024;

/// 文件名取内容哈希的前多少位。
const NAME_HASH_CHARS: usize = 16;

/// 一个收件箱目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInbox {
    dir: PathBuf,
}

impl ImageInbox {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
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

    /// 存一个不是图片的文件，保留原文件名（模型靠扩展名决定怎么打开它）：
    /// `{内容哈希}/{文件名}`。同一份内容同一个名字重传落在同一个文件上；名字里的目录部分与控制字符去掉。
    pub async fn save_file(&self, bytes: &[u8], name: &str) -> Result<PathBuf, String> {
        if bytes.len() > DOWNLOAD_LIMIT_BYTES {
            return Err(format!(
                "文件 {} 字节，超过 {} 字节的上限",
                bytes.len(),
                DOWNLOAD_LIMIT_BYTES
            ));
        }
        let hash = ContentHash::of_bytes(bytes);
        let dir = self.dir.join(&hash.as_str()[..NAME_HASH_CHARS]);
        let path = dir.join(file_name(name));
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(path);
        }
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| format!("建不了目录 {}：{error}", dir.display()))?;
        let partial = dir.join(format!(".{}.partial", hash.as_str()));
        tokio::fs::write(&partial, bytes)
            .await
            .map_err(|error| format!("写不了 {}：{error}", partial.display()))?;
        tokio::fs::rename(&partial, &path)
            .await
            .map_err(|error| format!("改名到 {} 失败：{error}", path.display()))?;
        Ok(path)
    }
}

/// 上传来的文件名只留最后一段、去掉控制字符，空的、`.`、`..` 换成 `file`；太长截到 120 个字符。
fn file_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = last.chars().filter(|c| !c.is_control()).take(120).collect();
    let clean = clean.trim();
    if clean.is_empty() || clean == "." || clean == ".." {
        "file".into()
    } else {
        clean.to_string()
    }
}

/// 一张存好的图在正文里的那行说明。网页输入框的附件（Gateway 的 `POST /v1/attachments`）用它与
/// [`file_note`]，写法只有这两处；网页按这两个形状把说明认回缩略图与文件标签
/// （`apps/client` 的 `features/conversation/attachments.ts`），改了要一起改。
pub fn image_note(path: &Path) -> String {
    format!(
        "[用户发来一张图片，已存到 {}，用 read 查看]",
        path.display()
    )
}

/// 一个存好的文件（不是图片）在正文里的那行说明。
pub fn file_note(path: &Path) -> String {
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    format!(
        "[用户发来一个文件 {name}，已存到 {}，用 read 查看]",
        path.display()
    )
}

/// 正文里的一行如果是附件说明（[`image_note`]、[`file_note`]），给它一个短名字：
/// `图片` / `文件 {文件名}`。不是说明就是 `None`。起临时标题时用它跳过这些行（整段路径当标题没法看）。
pub fn attachment_label(line: &str) -> Option<String> {
    let line = line.trim();
    if line.starts_with("[用户发来一张图片") && line.ends_with(']') {
        return Some("图片".into());
    }
    let rest = line.strip_prefix("[用户发来一个文件 ")?;
    let (name, _) = rest.split_once("，已存到 ")?;
    line.ends_with(']').then(|| format!("文件 {name}"))
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

    #[tokio::test]
    async fn a_file_keeps_its_name_under_its_content_hash() {
        let dir = tempfile::tempdir().expect("临时目录");
        let inbox = ImageInbox::new(dir.path().join("inbox/web"));

        let path = inbox
            .save_file(b"a,b\n1,2\n", "../../报表 2026.csv")
            .await
            .expect("存得下");
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("报表 2026.csv")
        );
        assert_eq!(
            path.parent().and_then(Path::parent),
            Some(inbox.dir()),
            "名字里的目录部分不算数"
        );
        assert_eq!(std::fs::read(&path).expect("读回"), b"a,b\n1,2\n");
        assert_eq!(
            inbox
                .save_file(b"a,b\n1,2\n", "报表 2026.csv")
                .await
                .expect("重传"),
            path
        );

        let empty = inbox.save_file(b"", "..").await.expect("空文件也收");
        assert_eq!(empty.file_name().and_then(|n| n.to_str()), Some("file"));
        assert_eq!(
            file_note(&path),
            format!(
                "[用户发来一个文件 报表 2026.csv，已存到 {}，用 read 查看]",
                path.display()
            )
        );
    }

    #[test]
    fn attachment_notes_are_recognised_by_their_label() {
        assert_eq!(
            attachment_label(&image_note(Path::new("/w/a.png"))).as_deref(),
            Some("图片")
        );
        assert_eq!(
            attachment_label(&file_note(Path::new("/w/abc/a.md"))).as_deref(),
            Some("文件 a.md")
        );
        assert_eq!(attachment_label("在吗"), None);
    }
}
