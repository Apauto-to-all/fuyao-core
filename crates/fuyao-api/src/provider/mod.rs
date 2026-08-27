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

/// 供应商 API 协议（wire 协议方言）
///
/// 决定供应商实例构造时走哪套 wire 实现（请求编码 / 响应解析 / SSE 解码）。
/// 协议是供应商级本质属性（「说什么话」），与 `options`（连接凭据：去哪、
/// 拿什么钥匙）分居两层。配置必填无缺省：`[providers.<id>]` 段缺失
/// `api_protocol` 键即配置错误（fail-loud，见 ADR-0002）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiProtocol {
    /// OpenAI Chat Completions 兼容协议
    OpenaiCompletions,
    /// OpenAI Responses 协议
    OpenaiResponses,
    /// Anthropic Messages 协议
    AnthropicMessages,
}

impl ApiProtocol {
    /// 全部合法配置取值（错误信息列值用，与 [`ApiProtocol::from_config_str`] 识别集一致）
    pub const ALL_CONFIG_STRS: &'static [&'static str] = &[
        "openai-completions",
        "openai-responses",
        "anthropic-messages",
    ];

    /// 配置字符串解析为枚举（与 [`ApiProtocol::as_config_str`] 互逆）
    ///
    /// 精确匹配三个合法取值；未命中返回 `None`，由调用方组织 fail-loud 错误。
    pub fn from_config_str(s: &str) -> Option<Self> {
        match s {
            "openai-completions" => Some(Self::OpenaiCompletions),
            "openai-responses" => Some(Self::OpenaiResponses),
            "anthropic-messages" => Some(Self::AnthropicMessages),
            _ => None,
        }
    }

    /// 枚举转为配置字符串（写盘 / 展示统一出口）
    pub fn as_config_str(&self) -> &'static str {
        match self {
            Self::OpenaiCompletions => "openai-completions",
            Self::OpenaiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
        }
    }
}

impl std::fmt::Display for ApiProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_config_str())
    }
}

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

    /// API 协议（wire 方言，决定实例构造分派；配置必填）
    pub api_protocol: ApiProtocol,

    /// 模型配置字典
    pub models: HashMap<String, Model>,

    /// 配置选项
    pub options: ProviderOptions,

    /// API Key 环境变量名列表
    pub api_key_env_vars: Vec<String>,
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
            api_protocol: ApiProtocol::OpenaiCompletions,
            models,
            options: ProviderOptions::default(),
            api_key_env_vars: Vec::new(),
        };
        assert!(provider.models.contains_key("qwen3.6-plus"));
    }

    /// 配置字符串与枚举双向互逆：全部合法取值往返一致
    #[test]
    fn api_protocol_roundtrips_config_str() {
        for config_str in ApiProtocol::ALL_CONFIG_STRS {
            let protocol = ApiProtocol::from_config_str(config_str)
                .unwrap_or_else(|| panic!("合法取值应可解析：{config_str}"));
            assert_eq!(protocol.as_config_str(), *config_str);
        }
    }

    /// 非法取值返回 None（fail-loud 判定由调用方组织错误信息）
    #[test]
    fn api_protocol_rejects_unknown_config_str() {
        for bad in ["openai", "anthropic", "chat-completions", ""] {
            assert!(
                ApiProtocol::from_config_str(bad).is_none(),
                "非法取值应拒绝：{bad}"
            );
        }
    }

    /// serde 序列化为 kebab-case（与配置拼写一致，管理列表 JSON 沿用同一形态）
    #[test]
    fn api_protocol_serializes_kebab_case() {
        assert_eq!(
            serde_json::to_string(&ApiProtocol::OpenaiCompletions).unwrap(),
            "\"openai-completions\""
        );
        assert_eq!(
            serde_json::to_string(&ApiProtocol::AnthropicMessages).unwrap(),
            "\"anthropic-messages\""
        );
    }

    /// Display 与配置字符串同形（日志直接打印枚举即得配置拼写）
    #[test]
    fn api_protocol_display_matches_config_str() {
        assert_eq!(ApiProtocol::OpenaiResponses.to_string(), "openai-responses");
    }
}
