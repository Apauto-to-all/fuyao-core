//! 入站图片节流（normalize）
//!
//! 图片字节以 base64 内联进 DB，存储膨胀靠**入站源头节流**控制：
//! 落库前对超限图片解码 → 等比缩放到 ≤2000px 最大边长 → JPEG 多档质量压缩，
//! 首个达标档即用。落库即压缩成品，一次压缩同时服务存储与发送。
//!
//! 容错取舍：
//! - 字节达标的图**原样保留**（压缩有损，不重复动）——不检查像素维度，
//!   真实图片 5MB base64 内通常不超 2000px，模型侧也会自行降采样，像素上限收益有限
//! - 压缩失败 / 压缩后仍超限 → 返回 `None`，调用方降级为占位文本（对话连续性优先）

use base64::Engine;
use fuyao_api::ImageContent;
use std::io::Cursor;

/// 图片最大边长（像素），超限等比缩放
const MAX_IMAGE_PIXELS: u32 = 2000;

/// 图片 base64 最大字节数，超限压缩
const MAX_IMAGE_BASE64_BYTES: usize = 5 * 1024 * 1024;

/// JPEG 压缩质量档位（降序尝试，首个达标即用）
const JPEG_QUALITIES: [u8; 5] = [80, 85, 70, 55, 40];

/// 等比缩放目标尺寸：最大边长 > 2000px 时缩到 2000px，否则原尺寸
fn scaled_dimensions(w: u32, h: u32) -> (u32, u32) {
    let max = w.max(h);
    if max <= MAX_IMAGE_PIXELS {
        return (w, h);
    }
    let scale = MAX_IMAGE_PIXELS as f64 / max as f64;
    (
        ((w as f64) * scale).max(1.0) as u32,
        ((h as f64) * scale).max(1.0) as u32,
    )
}

/// 入站节流：超限图压缩为达标 JPEG，达标图原样保留
///
/// 返回 `None` 表示无法得到达标图（解码失败 / 全档压缩仍超限），调用方降级。
pub(crate) fn normalize_image(img: &ImageContent) -> Option<ImageContent> {
    // 字节达标：原样保留（压缩有损，不重复动）
    if img.data.len() <= MAX_IMAGE_BASE64_BYTES {
        return Some(img.clone());
    }

    // 超限：解码 → 缩放 → JPEG 档位压缩
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&img.data)
        .ok()?;
    let dyn_img = image::load_from_memory(&bytes).ok()?;
    let (w, h) = (dyn_img.width(), dyn_img.height());
    let (nw, nh) = scaled_dimensions(w, h);
    let resized = if (nw, nh) != (w, h) {
        dyn_img.resize(nw, nh, image::imageops::FilterType::Lanczos3)
    } else {
        dyn_img
    };

    // 统一转 RGB 后按质量档位编码 JPEG，首个达标即用
    let rgb = resized.to_rgb8();
    let (rw, rh) = (rgb.width(), rgb.height());
    for quality in JPEG_QUALITIES {
        let mut buf = Vec::new();
        {
            let mut cursor = Cursor::new(&mut buf);
            let mut encoder =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, quality);
            encoder
                .encode(rgb.as_raw(), rw, rh, image::ExtendedColorType::Rgb8)
                .ok()?;
        }
        let data = base64::engine::general_purpose::STANDARD.encode(&buf);
        if data.len() <= MAX_IMAGE_BASE64_BYTES {
            return Some(ImageContent {
                mime_type: "image/jpeg".to_string(),
                data,
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造 N×N 小图（纯色，压缩率高，用于达标路径测试）
    fn small_png_base64(size: u32) -> String {
        let mut img = image::RgbImage::new(size, size);
        for (x, _, p) in img.enumerate_pixels_mut() {
            *p = image::Rgb([x as u8, 17, 200]);
        }
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("PNG 编码失败");
        base64::engine::general_purpose::STANDARD.encode(&buf)
    }

    /// 构造必然超限的大图：q100 JPEG 编码的伪随机噪声（噪声不可压，稳定 >5MB）
    fn oversized_image_base64(size: u32) -> String {
        let mut img = image::RgbImage::new(size, size);
        // 确定性伪随机填充（LCG），不引入 rand 依赖
        let mut state = 0x1234_5678u32;
        for p in img.pixels_mut() {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let v = (state >> 16) as u8;
            *p = image::Rgb([v, v.wrapping_mul(3), v.wrapping_add(5)]);
        }
        let mut buf = Vec::new();
        {
            let mut cursor = Cursor::new(&mut buf);
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, 100);
            encoder
                .encode(img.as_raw(), size, size, image::ExtendedColorType::Rgb8)
                .expect("JPEG 编码失败");
        }
        base64::engine::general_purpose::STANDARD.encode(&buf)
    }

    #[test]
    fn normalize_keeps_small_image_untouched() {
        // 字节达标：原样保留（mime / data 完全一致）
        let img = ImageContent {
            mime_type: "image/png".into(),
            data: small_png_base64(64),
        };
        assert!(img.data.len() < MAX_IMAGE_BASE64_BYTES);
        let out = normalize_image(&img).expect("达标图不应失败");
        assert_eq!(out.mime_type, "image/png");
        assert_eq!(out.data, img.data);
    }

    #[test]
    fn normalize_compresses_oversized_image() {
        // 超限大图：压缩后必须 ≤5MB base64，且 mime 转 JPEG
        let raw = oversized_image_base64(3000);
        assert!(
            raw.len() > MAX_IMAGE_BASE64_BYTES,
            "测试前置：原始图必须超限，实际 {}",
            raw.len()
        );
        let img = ImageContent {
            mime_type: "image/png".into(),
            data: raw,
        };
        let out = normalize_image(&img).expect("大图应能压缩");
        assert!(
            out.data.len() <= MAX_IMAGE_BASE64_BYTES,
            "压缩后应 ≤5MB，实际 {}",
            out.data.len()
        );
        assert_eq!(out.mime_type, "image/jpeg");
        // 压缩后必须能解码（JPEG 有效性）+ 尺寸达标
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&out.data)
            .unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        let (w, h) = (decoded.width(), decoded.height());
        assert!(
            w.max(h) <= MAX_IMAGE_PIXELS,
            "缩放后最大边应 ≤2000，实际 {w}x{h}"
        );
    }

    #[test]
    fn normalize_rejects_invalid_base64() {
        // 非法 base64 → None（调用方降级）
        let img = ImageContent {
            mime_type: "image/png".into(),
            data: "!!!not-base64!!!".repeat(1_000_000), // 超限 + 非法
        };
        assert!(normalize_image(&img).is_none());
    }

    #[test]
    fn normalize_rejects_undecodable_image() {
        // base64 合法但内容不是图片 → None
        let junk = base64::engine::general_purpose::STANDARD.encode(b"not an image at all");
        let img = ImageContent {
            mime_type: "image/png".into(),
            data: junk.repeat(300_000), // 超限 + 不可解码
        };
        assert!(normalize_image(&img).is_none());
    }

    #[test]
    fn scaled_dimensions_shrinks_only_oversized() {
        assert_eq!(scaled_dimensions(100, 200), (100, 200));
        assert_eq!(scaled_dimensions(2000, 1500), (2000, 1500));
        assert_eq!(scaled_dimensions(4000, 2000), (2000, 1000));
        assert_eq!(scaled_dimensions(8000, 4000), (2000, 1000));
        // 极端长条不压成 0
        let (w, h) = scaled_dimensions(1, 100_000);
        assert_eq!(w, 1);
        assert_eq!(h, 2000);
    }
}
