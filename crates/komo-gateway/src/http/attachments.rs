//! `POST /v1/attachments` 与 `GET /v1/attachments/{收件箱}/{文件名}`：网页输入框的附件。
//!
//! 走聊天渠道收图片的同一条路（`komo_channel::inbox`，§11.2）：存进 `{workspaces_dir}/inbox/web/`，
//! 回执里给拼进正文的那行说明，消息结构、受理与回放都不认识附件。取回只给收件箱里按内容哈希
//! 命名的图片（网页缩略图用，聊天渠道收的图也能看），别的路径一概 404——这不是一个读文件的口子。

use std::path::PathBuf;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use komo_channel::inbox::{ImageInbox, file_note, image_note};
use komo_kernel::protocol::http::UploadAttachmentResponse;
use komo_kernel::types::tool::image_mime;
use serde::Deserialize;

use super::Api;
use super::error::{ApiFailure, ApiResult};

/// 网页的附件落在哪个收件箱（与 `inbox/feishu` 等并列）。
const WEB_INBOX: &str = "web";

/// 请求体上限：与聊天渠道下载图片的上限相同。axum 默认只收 2 MB，路由上要单独放宽到这个数。
pub const BODY_LIMIT_BYTES: usize = komo_channel::inbox::DOWNLOAD_LIMIT_BYTES;

#[derive(Debug, Default, Deserialize)]
pub struct UploadQuery {
    /// 原文件名。图片用不上（按内容命名），其他文件靠它保留扩展名。
    #[serde(default)]
    name: Option<String>,
}

fn inbox_root(api: &Api) -> PathBuf {
    api.state.snapshot().paths.workspaces_dir.join("inbox")
}

/// 存下一个附件：认得出的图片按内容哈希存成 `{hash}.{ext}`，其他文件存成 `{hash}/{原文件名}`。
pub async fn upload(
    State(api): State<Api>,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> ApiResult<Json<UploadAttachmentResponse>> {
    let inbox = ImageInbox::new(inbox_root(&api).join(WEB_INBOX));
    if image_mime(&body).is_some() {
        let path = inbox.save(&body).await.map_err(ApiFailure::invalid)?;
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        return Ok(Json(UploadAttachmentResponse {
            note: image_note(&path),
            image_url: Some(format!("/v1/attachments/{WEB_INBOX}/{name}")),
            path: path.display().to_string(),
        }));
    }
    let name = query.name.unwrap_or_default();
    let path = inbox
        .save_file(&body, &name)
        .await
        .map_err(ApiFailure::invalid)?;
    Ok(Json(UploadAttachmentResponse {
        note: file_note(&path),
        image_url: None,
        path: path.display().to_string(),
    }))
}

/// 取回收件箱里的一张图。名字必须是 `{16 位十六进制}.{png|jpg|gif|webp}`，收件箱名只许小写字母、
/// 数字与 `-`/`_`：拼出来的路径出不了 `inbox/` 一层。
pub async fn image(
    State(api): State<Api>,
    Path((inbox, name)): Path<(String, String)>,
) -> ApiResult<Response> {
    if !valid_inbox(&inbox) || !valid_image_name(&name) {
        return Err(ApiFailure::not_found(format!("附件 {inbox}/{name}")));
    }
    let path = inbox_root(&api).join(&inbox).join(&name);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| ApiFailure::not_found(format!("附件 {inbox}/{name}")))?;
    let Some(mime) = image_mime(&bytes) else {
        return Err(ApiFailure::not_found(format!("附件 {inbox}/{name}")));
    };
    Ok((
        [
            (header::CONTENT_TYPE, mime),
            // 按内容命名：同一个地址永远是同一张图
            (
                header::CACHE_CONTROL,
                "private, max-age=31536000, immutable",
            ),
        ],
        bytes,
    )
        .into_response())
}

fn valid_inbox(inbox: &str) -> bool {
    !inbox.is_empty()
        && inbox.len() <= 32
        && inbox
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn valid_image_name(name: &str) -> bool {
    let Some((stem, extension)) = name.split_once('.') else {
        return false;
    };
    stem.len() == 16
        && stem
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        && matches!(extension, "png" | "jpg" | "gif" | "webp")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::test_support::TestGateway;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest-of-png";

    async fn upload_raw(gw: &TestGateway, query: &str, body: Vec<u8>) -> (u16, String) {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/attachments{query}", gw.base_url()))
            .bearer_auth(gw.token())
            .body(body)
            .send()
            .await
            .expect("请求发得出去");
        let status = response.status().as_u16();
        (status, response.text().await.unwrap_or_default())
    }

    #[tokio::test]
    async fn a_pasted_image_lands_in_the_web_inbox_and_comes_back_as_a_thumbnail() {
        let gw = TestGateway::start().await;
        let (status, body) = upload_raw(&gw, "?name=粘贴.png", PNG.to_vec()).await;
        assert_eq!(status, 200, "{body}");
        let receipt: UploadAttachmentResponse = serde_json::from_str(&body).expect("回执");
        let path = std::path::Path::new(&receipt.path);
        let inbox = gw.state().snapshot().paths.workspaces_dir.join("inbox/web");
        assert_eq!(path.parent(), Some(inbox.as_path()));
        assert_eq!(std::fs::read(path).expect("存下了"), PNG);
        assert_eq!(receipt.note, image_note(path), "说明与聊天渠道同一个写法");

        let url = receipt.image_url.expect("图片有缩略图地址");
        let response = reqwest::Client::new()
            .get(format!("{}{url}", gw.base_url()))
            .bearer_auth(gw.token())
            .send()
            .await
            .expect("取得回来");
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
        assert_eq!(response.bytes().await.expect("正文").as_ref(), PNG);
    }

    #[tokio::test]
    async fn another_file_keeps_its_name_and_is_not_served_back() {
        let gw = TestGateway::start().await;
        let (status, body) = upload_raw(&gw, "?name=notes.md", b"# hi\n".to_vec()).await;
        assert_eq!(status, 200, "{body}");
        let receipt: UploadAttachmentResponse = serde_json::from_str(&body).expect("回执");
        assert!(receipt.path.ends_with("/notes.md"), "{}", receipt.path);
        assert_eq!(receipt.image_url, None);
        assert!(receipt.note.contains("文件 notes.md"), "{}", receipt.note);

        let (status, _) = gw.get("/v1/attachments/web/..%2F..%2F.env").await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn a_body_over_the_limit_is_refused_before_it_is_stored() {
        let gw = TestGateway::start().await;
        let (status, _) = upload_raw(&gw, "", vec![0u8; BODY_LIMIT_BYTES + 1]).await;
        assert_eq!(status, 413);
        let inbox = gw.state().snapshot().paths.workspaces_dir.join("inbox/web");
        assert!(!inbox.exists(), "没存任何东西");
    }

    #[test]
    fn only_hashed_image_names_in_plain_inboxes_are_served() {
        assert!(valid_inbox("web"));
        assert!(valid_inbox("feishu"));
        assert!(!valid_inbox(".."));
        assert!(!valid_inbox("web/../x"));
        assert!(!valid_inbox(""));

        assert!(valid_image_name("0123456789abcdef.png"));
        assert!(!valid_image_name("0123456789ABCDEF.png"));
        assert!(!valid_image_name("0123456789abcdef.bin"));
        assert!(!valid_image_name("0123456789abcdef.png.partial"));
        assert!(!valid_image_name("../../.env"));
        assert!(!valid_image_name("0123456789abcdef/report.csv"));
    }
}
