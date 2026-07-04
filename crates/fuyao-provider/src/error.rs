//! Provider 错误类型

use thiserror::Error;

/// Provider 错误
#[derive(Debug, Error)]
pub enum ProviderError {
    /// Provider 不存在
    #[error("Provider 不存在: {0}")]
    ProviderNotFound(String),

    /// API Key 未配置
    #[error("Provider {provider} 的 API Key 未配置")]
    MissingApiKey {
        /// Provider ID
        provider: String,
    },

    /// 模型不存在
    #[error("模型不存在: {0}")]
    ModelNotFound(String),

    /// 模型 ID 格式错误
    #[error("模型 ID 格式错误，应为 provider_id/model_id: {0}")]
    InvalidModelId(String),
}
