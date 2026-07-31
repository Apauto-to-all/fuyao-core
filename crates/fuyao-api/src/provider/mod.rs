//! 模型和供应商模块
//!
//! 定义 LLM 模型配置和供应商信息。
//! - `model` 子模块：模型配置（Model、ModelCost、ModelLimit、ModelModalities、PriceTier）
//! - 本文件：供应商配置（Provider、ProviderOptions）
//!
//! 注：原 `provider/provider.rs`（嵌套同名模块，触发 clippy::module_inception）的
//! Provider/ProviderOptions 定义已上提至此文件，公共路径 `fuyao_api::provider::*` 不变。

pub mod model;

use std::collections::HashMap;

pub use model::{
    InputModality, Model, ModelCost, ModelLimit, ModelModalities, OutputModality, PriceTier,
    ThinkingType,
};

// === 供应商配置类型 ===

/// 供应商配置选项
///
/// 包含自定义 base URL 和 API Key。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ProviderOptions {
    /// 自定义 base URL
    pub base_url: Option<String>,

    /// API Key
    pub api_key: Option<String>,
}

/// 供应商配置
///
/// 定义一个 LLM 供应商的完整配置，包括模型列表、认证信息等。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Provider {
    /// 供应商显示名称
    pub name: String,

    /// 模型配置字典
    pub models: HashMap<String, Model>,

    /// 配置选项
    pub options: ProviderOptions,

    /// API Key 环境变量名列表
    pub api_key_env_vars: Vec<String>,
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            name: "".to_string(),
            models: HashMap::new(),
            options: ProviderOptions::default(),
            api_key_env_vars: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_options_default_has_no_values() {
        let options = ProviderOptions::default();
        assert!(options.base_url.is_none());
        assert!(options.api_key.is_none());
    }

    #[test]
    fn provider_with_models() {
        let mut models = HashMap::new();
        models.insert(
            "qwen3.6-plus".to_string(),
            Model {
                name: "qwen3.6-plus".to_string(),
                cost: ModelCost::default(),
                limit: ModelLimit::default(),
                reasoning_efforts: vec![],
                modalities: ModelModalities::default(),
            },
        );
        let provider = Provider {
            name: "aliyun".to_string(),
            models,
            options: ProviderOptions::default(),
            api_key_env_vars: Vec::new(),
        };
        assert!(provider.models.contains_key("qwen3.6-plus"));
    }
}
