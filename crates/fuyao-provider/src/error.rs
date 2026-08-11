//! Provider 错误类型

use thiserror::Error;

/// Provider 错误
///
/// 当前仅覆盖模型 ID 解析错误（`resolver::parse_model_id`）。
/// Provider 运行时（流式 / HTTP）错误另见 [`crate::StreamError`]。
#[derive(Debug, Error)]
pub enum ProviderError {
    /// 模型 ID 格式错误
    #[error("模型 ID 格式错误，应为 provider_id/model_id: {0}")]
    InvalidModelId(String),
}
