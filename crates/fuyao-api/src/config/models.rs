//! 多模型选择配置（按用途标签区分）
//!
//! 对应 `fuyao.toml` 中 `[models.{tag}]` 子段。轻量任务场景按标签选取专用模型，
//! 与主对话模型区分（主对话用强模型，标题 / 压缩等用便宜快速模型）。
//!
//! # 内置标签
//! - `fast`：轻量任务模型（标题生成、压缩总结等）。可选配置，未配置时轻量任务
//!   回退到当前会话模型。
//!
//! 主对话模型**不在此配置**——由调用方创建会话时在 `ModelConfig.model_id` 显式提供，
//! 未提供则引擎拒绝对话（fail-loud，见 `resolve_model`）。标签固定为 `fast`，
//! 配置未知标签会加载报错（`deny_unknown_fields`），避免死配置。
//!
//! # 配置示例
//! ```toml
//! [models.fast]
//! model = "deepseek/deepseek-v4-flash"
//! thinking_type = "Enabled"
//! reasoning_effort = "high"
//! ```

use crate::provider::ThinkingType;
use serde::Deserialize;

/// 模型引用配置
///
/// 对应 `fuyao.toml` 中 `[models.{tag}]` 子段，按用途标签配置一个模型引用。
/// 承载模型 ID + 思考运行参数（思考开关 / 思考强度档位）。
///
/// 标签无关——任何 `[models.{tag}]` 子段都用此结构，三字段均可选：
/// 缺省即 None，请求体不发对应字段，走模型自身默认行为。
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
/// 对应 `fuyao.toml` 中 `[models]` 段。标签固定为 `fast`，配置未知标签会加载报错
/// （`deny_unknown_fields`），避免配了不生效的死配置。
///
/// 主对话模型不在配置内——创建会话时由 `ModelConfig.model_id` 显式指定，
/// 引擎不提供任何隐式兜底。加新标签时在此结构体加 `Option<ModelRef>` 字段，TOML 格式不变。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelSelection {
    /// 轻量任务模型（标题生成、压缩总结等）。可选，未配置时轻量任务回退到当前会话模型
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
    fn model_selection_default_is_fast_none() {
        let s = ModelSelection::default();
        assert!(s.fast.is_none());
    }

    /// [models.fast] 反序列化
    #[test]
    fn model_selection_deserialize() {
        let toml_str = r#"
[models.fast]
model = "deepseek/deepseek-v4-flash"
"#;
        #[derive(Deserialize)]
        struct Wrapper {
            models: ModelSelection,
        }
        let w: Wrapper = toml::from_str(toml_str).unwrap();
        assert_eq!(w.models.fast.unwrap().model, "deepseek/deepseek-v4-flash");
    }

    /// thinking_type / reasoning_effort 反序列化（默认 None，配了才解析）
    #[test]
    fn model_ref_deserialize_thinking_fields() {
        let toml_str = r#"
[models.fast]
model = "deepseek/deepseek-v4-flash"
thinking_type = "Enabled"
reasoning_effort = "high"
"#;
        #[derive(Deserialize)]
        struct Wrapper {
            models: ModelSelection,
        }
        let w: Wrapper = toml::from_str(toml_str).unwrap();
        let fast = w.models.fast.unwrap();
        assert_eq!(fast.model, "deepseek/deepseek-v4-flash");
        assert_eq!(fast.thinking_type, Some(ThinkingType::Enabled));
        assert_eq!(fast.reasoning_effort.as_deref(), Some("high"));
    }

    /// thinking 字段缺省 → None（不强制配置）
    #[test]
    fn model_ref_thinking_fields_default_none() {
        let r = ModelRef::default();
        assert!(r.thinking_type.is_none());
        assert!(r.reasoning_effort.is_none());
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

    /// `[models.default]` 被拒绝——主对话模型不在配置内，由 ModelConfig.model_id 显式指定
    ///
    /// 该标签曾用于配置全局兜底模型，现已移除；保留此断言作为回归保护，
    /// 防止旧配置文件带 `[models.default]` 段时被静默吞掉（应加载失败让用户感知）。
    #[test]
    fn model_selection_rejects_default_tag() {
        let toml_str = r#"
[models.default]
model = "deepseek/deepseek-v4-flash"
"#;
        #[derive(Deserialize)]
        struct Wrapper {
            #[expect(dead_code)]
            models: ModelSelection,
        }
        let result: Result<Wrapper, _> = toml::from_str(toml_str);
        assert!(
            result.is_err(),
            "[models.default] 应被 deny_unknown_fields 拒绝"
        );
    }
}
