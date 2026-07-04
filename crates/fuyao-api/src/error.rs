//! API 错误类型定义

use thiserror::Error;

/// API 错误类型
#[derive(Error, Debug)]
pub enum ApiError {
    #[error("请求失败: {0}")]
    RequestFailed(String),

    #[error("模型不存在: {model}")]
    ModelNotFound { model: String },

    #[error("解析错误: {0}")]
    ParseError(#[from] serde_json::Error),

    #[error("配置错误: {0}")]
    ConfigError(String),

    #[error("会话不存在: {0}")]
    SessionNotFound(String),

    #[error("工具执行失败: {tool}: {reason}")]
    ToolError { tool: String, reason: String },

    #[error("IO 错误: {0}")]
    IoError(#[from] std::io::Error),
}
