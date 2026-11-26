//! 给模型的图片超过上限时缩小（pi-durable 1.6 的 `images/fit`）。
//!
//! 用 `image`（§13.4，只开 PNG / JPEG / GIF / WebP）：解码、按 EXIF 方向摆正、透明铺到白底、
//! 长边逐级变短、编成 JPEG，直到放得下 [`IMAGE_LIMIT_BYTES`]。重新编码不带 EXIF。动图只取第一帧。
//! 没超限的图一个字节都不动。解码按 `image` 的默认上限（单张至多分配 512 MiB），防解压炸弹。

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, RgbImage};
use komo_kernel::types::tool::{IMAGE_LIMIT_BYTES, ToolImage};

/// 依次试的长边（像素）。原图更短的那几档跳过。
const EDGES: [u32; 5] = [2048, 1600, 1200, 800, 512];
/// JPEG 质量。
const QUALITY: u8 = 80;

/// 放得下就原样交回；放不下就缩，缩不下来是 `Err(原因)`。解码与编码是 CPU 活，放在阻塞线程上。
pub async fn fit(image: ToolImage) -> Result<ToolImage, String> {
    if image.bytes.len() <= IMAGE_LIMIT_BYTES {
        return Ok(image);
    }
    tokio::task::spawn_blocking(move || shrink(&image))
        .await
        .map_err(|error| format!("缩图的线程没跑完：{error}"))?
}

fn shrink(image: &ToolImage) -> Result<ToolImage, String> {
    let decoded = decode(image)?;
    let flat = on_white(&decoded);
    let longest = flat.width().max(flat.height());
    let mut last = String::new();
    // 原图本来就不大（只是编码得胖）：先按原尺寸重编一次。
    let edges = std::iter::once(longest).chain(EDGES.into_iter().filter(|edge| *edge < longest));
    for edge in edges {
        let resized = if edge < longest {
            DynamicImage::ImageRgb8(flat.clone())
                .resize(edge, edge, FilterType::Triangle)
                .into_rgb8()
        } else {
            flat.clone()
        };
        let bytes = encode(&resized)?;
        if bytes.len() <= IMAGE_LIMIT_BYTES {
            tracing::debug!(
                from = image.bytes.len(),
                to = bytes.len(),
                edge,
                "图片缩小后交给模型"
            );
            return Ok(ToolImage {
                mime: "image/jpeg".into(),
                bytes,
            });
        }
        last = format!("长边 {edge} 像素仍有 {} 字节", bytes.len());
    }
    Err(format!("没能缩到上限以内：{last}"))
}

/// 解码并按 EXIF 方向摆正。格式按 mime 定（调用方已经按文件头认过），认不了的再猜一次。
fn decode(image: &ToolImage) -> Result<DynamicImage, String> {
    let mut reader = ImageReader::new(Cursor::new(&image.bytes));
    match ImageFormat::from_mime_type(&image.mime) {
        Some(format) => reader.set_format(format),
        None => {
            reader = reader
                .with_guessed_format()
                .map_err(|error| format!("认不出图片格式：{error}"))?;
        }
    }
    let mut decoder = reader
        .into_decoder()
        .map_err(|error| format!("解不开这张 {}：{error}", image.mime))?;
    let orientation = decoder.orientation().ok();
    let mut decoded = DynamicImage::from_decoder(decoder)
        .map_err(|error| format!("解不开这张 {}：{error}", image.mime))?;
    if let Some(orientation) = orientation {
        decoded.apply_orientation(orientation);
    }
    Ok(decoded)
}

/// JPEG 没有透明：按 alpha 铺到白底上。
fn on_white(image: &DynamicImage) -> RgbImage {
    if !image.color().has_alpha() {
        return image.to_rgb8();
    }
    let rgba = image.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let blend =
            |c: u8| ((u16::from(c) * u16::from(a) + 255 * (255 - u16::from(a))) / 255) as u8;
        image::Rgb([blend(r), blend(g), blend(b)])
    })
}

fn encode(image: &RgbImage) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, QUALITY)
        .encode_image(image)
        .map_err(|error| format!("编不成 JPEG：{error}"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::types::tool::image_mime;

    /// 一张压不小的 PNG：每个像素都不一样（简单的线性同余噪点，确定性）。
    fn noisy_png(width: u32, height: u32, alpha: bool) -> Vec<u8> {
        let mut state: u32 = 12345;
        let mut next = || {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (state >> 16) as u8
        };
        let image = if alpha {
            DynamicImage::ImageRgba8(image::RgbaImage::from_fn(width, height, |_, _| {
                image::Rgba([next(), next(), next(), next()])
            }))
        } else {
            DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |_, _| {
                image::Rgb([next(), next(), next()])
            }))
        };
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

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

    #[tokio::test]
    async fn an_oversized_image_is_shrunk_to_a_jpeg_within_the_limit() {
        let bytes = noisy_png(1400, 1400, false);
        assert!(bytes.len() > IMAGE_LIMIT_BYTES, "{}", bytes.len());
        let fitted = fit(ToolImage {
            mime: "image/png".into(),
            bytes,
        })
        .await
        .unwrap();
        assert_eq!(fitted.mime, "image/jpeg");
        assert_eq!(image_mime(&fitted.bytes), Some("image/jpeg"));
        assert!(fitted.bytes.len() <= IMAGE_LIMIT_BYTES);
    }

    #[tokio::test]
    async fn a_very_large_image_has_its_long_edge_cut_and_keeps_its_aspect() {
        let bytes = noisy_png(4000, 2000, true);
        let fitted = fit(ToolImage {
            mime: "image/png".into(),
            bytes,
        })
        .await
        .unwrap();
        let decoded = image::load_from_memory(&fitted.bytes).unwrap();
        assert!(decoded.width() <= 2048, "{}", decoded.width());
        assert_eq!(decoded.width(), decoded.height() * 2);
    }

    #[test]
    fn transparency_is_laid_on_white() {
        let clear = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            1,
            1,
            image::Rgba([0, 0, 0, 0]),
        ));
        assert_eq!(on_white(&clear).get_pixel(0, 0).0, [255, 255, 255]);
    }
}
