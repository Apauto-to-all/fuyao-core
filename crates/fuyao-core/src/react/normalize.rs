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

/// 等比缩放目标尺寸：最大边长超限缩到 max_pixels，否则原尺寸
fn scaled_dimensions(w: u32, h: u32, max_pixels: u32) -> (u32, u32) {
    let max = w.max(h);
    if max <= max_pixels {
        return (w, h);
    }
    let scale = max_pixels as f64 / max as f64;
    (
        ((w as f64) * scale).max(1.0) as u32,
        ((h as f64) * scale).max(1.0) as u32,
    )
}

/// 入站节流：超限图压缩为达标 JPEG，达标图原样保留
///
/// 返回 `None` 表示无法得到达标图（解码失败 / 全档压缩仍超限），调用方降级。
pub(crate) fn normalize_image(img: &ImageContent) -> Option<ImageContent> {
    let cfg = fuyao_api::get_config();

    // 入站归一：data URL → 裸 base64（统一后续处理基准与落库形态）
    // 非 data URL（裸 base64）from_data_url 返回 None，沿用入参原值
    let img = ImageContent::from_data_url(&img.data).unwrap_or_else(|| img.clone());

    // 字节达标：原样保留（压缩有损，不重复动）
    if img.data.len() <= cfg.image.max_base64_bytes {
        return Some(img);
    }

    // 关闭压缩：超限图原样保留（仍含上方 data URL 归一结果），存储膨胀与发送超限风险由用户自负
    if !cfg.image.compress {
        return Some(img);
    }

    // 超限：解码 → 缩放 → JPEG 档位压缩
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&img.data)
        .ok()?;
    let dyn_img = image::load_from_memory(&bytes).ok()?;
    let (w, h) = (dyn_img.width(), dyn_img.height());
    let (nw, nh) = scaled_dimensions(w, h, cfg.image.max_pixels);
    let resized = if (nw, nh) != (w, h) {
        dyn_img.resize(nw, nh, image::imageops::FilterType::Lanczos3)
    } else {
        dyn_img
    };

    // 统一转 RGB 后按质量档位编码 JPEG，首个达标即用
    let rgb = resized.to_rgb8();
    let (rw, rh) = (rgb.width(), rgb.height());
    for quality in &cfg.image.jpeg_qualities {
        let mut buf = Vec::new();
        {
            let mut cursor = Cursor::new(&mut buf);
            let mut encoder =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, *quality);
            encoder
                .encode(rgb.as_raw(), rw, rh, image::ExtendedColorType::Rgb8)
                .ok()?;
        }
        let data = base64::engine::general_purpose::STANDARD.encode(&buf);
        if data.len() <= cfg.image.max_base64_bytes {
            return Some(ImageContent {
                mime_type: "image/jpeg".to_string(),
                data,
            });
        }
    }

    None
}

/// 批量入站节流：逐图归一 data URL + 超限压缩，返回 (达标图, 失败计数)
///
/// 失败 = 解码失败 / 压缩后仍超限；失败图不进达标列表，调用方按 `failed` 计数
/// 决定是否附加占位文本。整批在同一阻塞线程内处理（由调用方经 `spawn_blocking` 调入），
/// 避免每张图各自 spawn 一次任务。
pub(crate) fn normalize_images(images: &[ImageContent]) -> (Vec<ImageContent>, usize) {
    let mut kept = Vec::with_capacity(images.len());
    let mut failed = 0usize;
    for img in images {
        match normalize_image(img) {
            Some(n) => kept.push(n),
            None => failed += 1,
        }
    }
    (kept, failed)
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
        let max_bytes = fuyao_api::get_config().image.max_base64_bytes;
        // 字节达标：原样保留（mime / data 完全一致）
        let img = ImageContent {
            mime_type: "image/png".into(),
            data: small_png_base64(64),
        };
        assert!(img.data.len() < max_bytes);
        let out = normalize_image(&img).expect("达标图不应失败");
        assert_eq!(out.mime_type, "image/png");
        assert_eq!(out.data, img.data);
    }

    #[test]
    fn normalize_compresses_oversized_image() {
        let cfg = fuyao_api::get_config();
        let max_bytes = cfg.image.max_base64_bytes;
        let max_pixels = cfg.image.max_pixels;
        // 超限大图：压缩后必须 ≤5MB base64，且 mime 转 JPEG
        let raw = oversized_image_base64(3000);
        assert!(
            raw.len() > max_bytes,
            "测试前置：原始图必须超限，实际 {}",
            raw.len()
        );
        let img = ImageContent {
            mime_type: "image/png".into(),
            data: raw,
        };
        let out = normalize_image(&img).expect("大图应能压缩");
        assert!(
            out.data.len() <= max_bytes,
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
            w.max(h) <= max_pixels,
            "缩放后最大边应 ≤{max_pixels}，实际 {w}x{h}"
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
        let max_pixels = 2000;
        assert_eq!(scaled_dimensions(100, 200, max_pixels), (100, 200));
        assert_eq!(scaled_dimensions(2000, 1500, max_pixels), (2000, 1500));
        assert_eq!(scaled_dimensions(4000, 2000, max_pixels), (2000, 1000));
        assert_eq!(scaled_dimensions(8000, 4000, max_pixels), (2000, 1000));
        // 极端长条不压成 0
        let (w, h) = scaled_dimensions(1, 100_000, max_pixels);
        assert_eq!(w, 1);
        assert_eq!(h, 2000);
    }

    #[test]
    fn normalize_image_accepts_data_url_input() {
        // data URL 入站：归一为裸 base64，mime 取自 data URL 内嵌声明
        let png = small_png_base64(64);
        let img = ImageContent {
            mime_type: String::new(),
            data: format!("data:image/png;base64,{png}"),
        };
        let out = normalize_image(&img).expect("data URL 达标图应成功");
        assert_eq!(out.mime_type, "image/png");
        assert_eq!(out.data, png, "应剥离 data: 前缀保留裸 base64");
    }

    #[test]
    fn normalize_images_batch_collects_kept_and_failed() {
        // 一张达标裸 base64 + 一张超限且不可解码 → kept=1, failed=1
        let good = ImageContent {
            mime_type: "image/png".into(),
            data: small_png_base64(32),
        };
        let bad = ImageContent {
            mime_type: "image/png".into(),
            data: base64::engine::general_purpose::STANDARD
                .encode(b"not an image")
                .repeat(400_000),
        };
        let (kept, failed) = normalize_images(&[good, bad]);
        assert_eq!(kept.len(), 1);
        assert_eq!(failed, 1);
    }
}
