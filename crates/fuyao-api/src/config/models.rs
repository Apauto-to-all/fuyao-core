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
