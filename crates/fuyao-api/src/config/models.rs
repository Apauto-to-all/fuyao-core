//! 多模型选择配置（按用途标签区分）
//!
//! 对应 `fuyao.toml` 中 `[models.{tag}]` 子段。不同用途的场景按标签选取模型，
//! 实现主对话用强模型、轻量任务用便宜模型的区分。
//!
//! # 内置标签
//! - `default`：默认模型（主对话）。未显式指定 model_id 时使用。
//! - `fast`：轻量任务模型（标题生成、压缩总结等）。便宜快速，不可用时回退 `default`。
//!
//! # 配置示例
//! ```toml
//! [models.default]
//! model = "deepseek/deepseek-v4-flash"
//!
//! [models.fast]
//! model = "deepseek/deepseek-v4-flash"
//! ```

use serde::Deserialize;

/// 模型引用配置
///
/// 对应 `fuyao.toml` 中 `[models.{tag}]` 子段，按用途标签配置一个模型引用。
/// 当前仅承载模型 ID，未来可按需扩展思考开关等运行参数。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ModelRef {
    /// 模型 ID（格式：provider_id/model_id）
    pub model: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ref_default_has_empty_model() {
        let r = ModelRef::default();
        assert!(r.model.is_empty());
    }

    /// [models.default] 子段反序列化为 HashMap<String, ModelRef>
    #[test]
    fn model_ref_deserialize_from_subsection() {
        let toml_str = r#"
[models.default]
model = "deepseek/deepseek-v4-flash"

[models.fast]
model = "deepseek/deepseek-v4-flash"
"#;
        #[derive(Deserialize)]
        struct Wrapper {
            models: std::collections::HashMap<String, ModelRef>,
        }
        let w: Wrapper = toml::from_str(toml_str).unwrap();
        assert_eq!(w.models["default"].model, "deepseek/deepseek-v4-flash");
        assert_eq!(w.models["fast"].model, "deepseek/deepseek-v4-flash");
    }
}
