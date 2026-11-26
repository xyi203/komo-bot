//! 给模型的图片超过上限时缩小（pi-durable 1.6 的 `images/fit`）。
//!
//! 不引入 `image`（§13.4，冷编预算）：借本机已有的转换器——macOS 自带的 `sips`，其次是
//! ImageMagick（`magick`，老版本叫 `convert`）。缩成 JPEG、长边逐级变短，直到放得下
//! [`IMAGE_LIMIT_BYTES`]；ImageMagick 顺手去掉 EXIF 并按方向摆正。都没有或都失败时原样交回错误，
//! 调用方照旧报"太大"。没超限的图一个字节都不动。

use std::path::Path;
use std::time::Duration;

use komo_kernel::types::tool::{IMAGE_LIMIT_BYTES, ToolImage, image_mime};
use tokio::process::Command;

/// 依次试的长边（像素）。
const EDGES: [u32; 4] = [2048, 1600, 1200, 800];
/// 一次转换最多等多久。
const CONVERT_TIMEOUT: Duration = Duration::from_secs(30);

/// 放得下就原样交回；放不下就缩，缩不下来是 `Err(原因)`。
pub async fn fit(image: ToolImage) -> Result<ToolImage, String> {
    if image.bytes.len() <= IMAGE_LIMIT_BYTES {
        return Ok(image);
    }
    let Some(converter) = Converter::find() else {
        return Err("本机没有 sips 或 ImageMagick，缩不了图".into());
    };
    let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
    let input = dir.path().join(format!("in.{}", extension(&image.mime)));
    tokio::fs::write(&input, &image.bytes)
        .await
        .map_err(|error| error.to_string())?;
    let mut last = String::new();
    for edge in EDGES {
        let output = dir.path().join(format!("out-{edge}.jpg"));
        if let Err(error) = converter.shrink(&input, &output, edge).await {
            last = error;
            break;
        }
        let bytes = tokio::fs::read(&output)
            .await
            .map_err(|error| error.to_string())?;
        if image_mime(&bytes) != Some("image/jpeg") {
            last = format!("{} 交回的不是 JPEG", converter.name());
            break;
        }
        if bytes.len() <= IMAGE_LIMIT_BYTES {
            tracing::debug!(
                from = image.bytes.len(),
                to = bytes.len(),
                edge,
                converter = converter.name(),
                "图片缩小后交给模型"
            );
            return Ok(ToolImage {
                mime: "image/jpeg".into(),
                bytes,
            });
        }
        last = format!("长边 {edge} 像素仍有 {} 字节", bytes.len());
    }
    Err(format!("{} 没能缩到上限以内：{last}", converter.name()))
}

fn extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

#[derive(Debug, Clone, Copy)]
enum Converter {
    Sips,
    Magick,
    Convert,
}

impl Converter {
    fn find() -> Option<Self> {
        if cfg!(target_os = "macos") && Path::new("/usr/bin/sips").is_file() {
            return Some(Self::Sips);
        }
        [Self::Magick, Self::Convert]
            .into_iter()
            .find(|converter| on_path(converter.name()))
    }

    fn name(self) -> &'static str {
        match self {
            Self::Sips => "sips",
            Self::Magick => "magick",
            Self::Convert => "convert",
        }
    }

    async fn shrink(self, input: &Path, output: &Path, edge: u32) -> Result<(), String> {
        let mut command = match self {
            Self::Sips => {
                let mut command = Command::new("/usr/bin/sips");
                command
                    .args(["-Z", &edge.to_string()])
                    .args(["-s", "format", "jpeg", "-s", "formatOptions", "80"])
                    .arg(input)
                    .arg("--out")
                    .arg(output);
                command
            }
            Self::Magick | Self::Convert => {
                let mut command = Command::new(self.name());
                // `[0]`：动图只取第一帧。`>`：只缩不放。
                let mut first = input.as_os_str().to_owned();
                first.push("[0]");
                command
                    .arg(first)
                    .args(["-auto-orient", "-strip"])
                    .args(["-background", "white", "-flatten"])
                    .args(["-resize", &format!("{edge}x{edge}>")])
                    .args(["-quality", "80"])
                    .arg(output);
                command
            }
        };
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let run = command.output();
        let finished = tokio::time::timeout(CONVERT_TIMEOUT, run)
            .await
            .map_err(|_| {
                format!(
                    "{} 超过 {} 秒没跑完",
                    self.name(),
                    CONVERT_TIMEOUT.as_secs()
                )
            })?
            .map_err(|error| format!("{} 起不来：{error}", self.name()))?;
        if finished.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&finished.stderr);
            Err(format!(
                "{} 失败（{}）：{}",
                self.name(),
                finished.status,
                stderr.lines().next().unwrap_or_default()
            ))
        }
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_image_within_the_limit_is_left_untouched() {
        let image = ToolImage {
            mime: "image/png".into(),
            bytes: b"\x89PNG\r\n\x1a\nsmall".to_vec(),
        };
        assert_eq!(fit(image.clone()).await.unwrap(), image);
    }

    #[tokio::test]
    async fn bytes_that_are_not_an_image_cannot_be_shrunk() {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(IMAGE_LIMIT_BYTES + 1, 0);
        let image = ToolImage {
            mime: "image/png".into(),
            bytes,
        };
        assert!(fit(image).await.is_err());
    }

    /// 一张真的大图（随机噪点的 PNG，压不小）缩成放得下的 JPEG。造图要 ImageMagick，没有就跳过。
    #[tokio::test]
    async fn an_oversized_image_is_shrunk_to_a_jpeg_within_the_limit() {
        if !on_path("magick") {
            eprintln!("没有 magick，跳过");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("noise.png");
        let status = std::process::Command::new("magick")
            .args(["-size", "1400x1400", "xc:", "+noise", "Random"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() > IMAGE_LIMIT_BYTES, "{}", bytes.len());
        let fitted = fit(ToolImage {
            mime: "image/png".into(),
            bytes,
        })
        .await
        .unwrap();
        assert_eq!(fitted.mime, "image/jpeg");
        assert!(fitted.bytes.len() <= IMAGE_LIMIT_BYTES);
    }
}
