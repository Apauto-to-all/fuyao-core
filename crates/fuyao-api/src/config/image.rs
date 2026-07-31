//! 图片处理配置
//!
//! 跨 crate 的图片处理可调参数集中于此，原散落在 `normalize.rs`（入站节流）
//! 与 `compressor/window.rs`（压缩 token 估算）的硬编码。

use serde::Deserialize;

/// 图片处理配置（入站节流 + 压缩 token 估算）
///
/// 跨 crate 的图片处理可调参数（入站节流策略 + 压缩 token 估算）。协议侧硬约束
/// （如 OpenAI 的 MIME 白名单、20MB 协议上限）属供应商协议事实，不在此配置——
/// 用户配了供应商也不认。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ImageConfig {
    /// 超限图片是否压缩节流（默认开）。关闭则超限图原样落库（仍做 data URL 归一），
    /// 存储膨胀与发送超限风险由用户自负。
    pub compress: bool,
    /// 图片最大边长（像素），超限等比缩放，原 `normalize.rs MAX_IMAGE_PIXELS = 2000`
    pub max_pixels: u32,
    /// base64 字节上限，超限触发解码 + 压缩，原 `normalize.rs MAX_IMAGE_BASE64_BYTES = 5242880`
    pub max_base64_bytes: usize,
    /// JPEG 压缩质量档位（降序尝试，首个达标即用），原 `normalize.rs JPEG_QUALITIES = [85,80,70,55,40]`
    pub jpeg_qualities: Vec<u8>,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            compress: true,
            max_pixels: 2000,
            max_base64_bytes: 5 * 1024 * 1024,
            jpeg_qualities: vec![85, 80, 70, 55, 40],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_hardcoded() {
        let c = ImageConfig::default();
        assert!(c.compress);
        assert_eq!(c.max_pixels, 2000);
        assert_eq!(c.max_base64_bytes, 5242880);
        assert_eq!(c.jpeg_qualities, vec![85, 80, 70, 55, 40]);
    }

    /// 反序列化：部分字段覆盖，缺省走 Default
    #[test]
    fn deserialize_partial() {
        let toml_str = r#"
[image]
compress = false
max_pixels = 3000
"#;
        #[derive(Deserialize)]
        struct Wrap {
            image: ImageConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert!(!w.image.compress);
        assert_eq!(w.image.max_pixels, 3000);
        // 缺省字段回退 default
        assert_eq!(w.image.max_base64_bytes, 5242880);
        assert_eq!(w.image.jpeg_qualities, vec![85, 80, 70, 55, 40]);
    }

    /// 完全缺省 `[image]` 段时整体走 Default
    #[test]
    fn deserialize_absent_uses_default() {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Wrap {
            image: ImageConfig,
        }
        let w: Wrap = toml::from_str("").unwrap();
        assert!(w.image.compress);
        assert_eq!(w.image.max_pixels, 2000);
        assert_eq!(w.image.max_base64_bytes, 5242880);
        assert_eq!(w.image.jpeg_qualities, vec![85, 80, 70, 55, 40]);
    }
}
