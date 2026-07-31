//! 多模型选择配置（按用途标签区分）
//!
//! 对应 `fuyao.toml` 中 `[models.{tag}]` 子段。不同用途的场景按标签选取模型，
//! 实现主对话用强模型、轻量任务用便宜模型的区分。
//!
//! # 内置标签
//! - `default`：默认模型（主对话）。未显式指定 model_id 时使用。
//! - `fast`：轻量任务模型（标题生成、压缩总结等）。便宜快速，不可用时回退 `default`。
//!
//! 标签固定为 `default` / `fast`，配置未知标签会加载报错（`deny_unknown_fields`），
//! 避免死配置。加新标签时在 [`ModelSelection`] 加 `Option<ModelRef>` 字段，TOML 格式不变。
//!
//! # 配置示例
//! ```toml
//! [models.default]
//! model = "deepseek/deepseek-v4-flash"
//! thinking_type = "enabled"
//! reasoning_effort = "high"
//!
//! [models.fast]
//! model = "deepseek/deepseek-v4-flash"
//! ```

use crate::provider::ThinkingType;
use serde::Deserialize;

/// 模型引用配置
///
/// 对应 `fuyao.toml` 中 `[models.{tag}]` 子段，按用途标签配置一个模型引用。
/// 承载模型 ID + 思考运行参数（思考开关 / 思考强度档位）。
///
/// default 与 fast 同结构——任何标签都支持全部字段，新增标签无需适配。
/// 三字段均可选：缺省即 None，请求体不发对应字段，走模型自身默认行为。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ModelRef {
    /// 模型 ID（格式：provider_id/model_id）
    pub model: String,

    /// 思考开关（对应 thinking.type 字段）。None 时不发，走模型默认
    pub thinking_type: Option<ThinkingType>,

    /// 思考强度档位名（用户自定义字符串，透传给服务器）。None 时不发，走模型默认
    pub reasoning_effort: Option<String>,
}

/// 多模型选择（固定标签）
///
/// 对应 `fuyao.toml` 中 `[models]` 段。标签固定为 `default` / `fast`，
/// 配置未知标签会加载报错（`deny_unknown_fields`），避免配了不生效的死配置。
///
/// 加新标签时在此结构体加 `Option<ModelRef>` 字段，TOML 格式不变。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelSelection {
    /// 默认模型（主对话）。未显式指定 model_id 时使用
    pub default: Option<ModelRef>,

    /// 轻量任务模型（标题生成、压缩总结等）。不可用时回退 `default`
    pub fast: Option<ModelRef>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ref_default_has_empty_model() {
        let r = ModelRef::default();
        assert!(r.model.is_empty());
    }

    #[test]
    fn model_selection_default_is_all_none() {
        let s = ModelSelection::default();
        assert!(s.default.is_none());
        assert!(s.fast.is_none());
    }

    /// [models.default] / [models.fast] 反序列化
    #[test]
    fn model_selection_deserialize() {
        let toml_str = r#"
[models.default]
model = "deepseek/deepseek-v4-flash"

[models.fast]
model = "deepseek/deepseek-v4-flash"
"#;
        #[derive(Deserialize)]
        struct Wrapper {
            models: ModelSelection,
        }
        let w: Wrapper = toml::from_str(toml_str).unwrap();
        assert_eq!(
            w.models.default.unwrap().model,
            "deepseek/deepseek-v4-flash"
        );
        assert_eq!(w.models.fast.unwrap().model, "deepseek/deepseek-v4-flash");
    }

    /// thinking_type / reasoning_effort 反序列化（默认 None，配了才解析）
    #[test]
    fn model_ref_deserialize_thinking_fields() {
        let toml_str = r#"
[models.default]
model = "deepseek/deepseek-v4-flash"
thinking_type = "enabled"
reasoning_effort = "high"
"#;
        #[derive(Deserialize)]
        struct Wrapper {
            models: ModelSelection,
        }
        let w: Wrapper = toml::from_str(toml_str).unwrap();
        let default = w.models.default.unwrap();
        assert_eq!(default.model, "deepseek/deepseek-v4-flash");
        assert_eq!(default.thinking_type, Some(ThinkingType::Enabled));
        assert_eq!(default.reasoning_effort.as_deref(), Some("high"));
    }

    /// thinking 字段缺省 → None（不强制配置）
    #[test]
    fn model_ref_thinking_fields_default_none() {
        let r = ModelRef::default();
        assert!(r.thinking_type.is_none());
        assert!(r.reasoning_effort.is_none());
    }

    /// 只配 default，fast 缺失 → fast 为 None
    #[test]
    fn model_selection_deserialize_partial() {
        let toml_str = r#"
[models.default]
model = "deepseek/deepseek-v4-flash"
"#;
        #[derive(Deserialize)]
        struct Wrapper {
            models: ModelSelection,
        }
        let w: Wrapper = toml::from_str(toml_str).unwrap();
        assert!(w.models.default.is_some());
        assert!(w.models.fast.is_none());
    }

    /// 未知标签报错（deny_unknown_fields）
    #[test]
    fn model_selection_rejects_unknown_tag() {
        let toml_str = r#"
[models.vision]
model = "deepseek/deepseek-v4-flash"
"#;
        #[derive(Deserialize)]
        struct Wrapper {
            // 此测试仅断言反序列化失败，字段不会被读取；保留以匹配 TOML 的 `models` 键结构
            #[expect(dead_code)]
            models: ModelSelection,
        }
        let result: Result<Wrapper, _> = toml::from_str(toml_str);
        assert!(result.is_err());
    }
}
