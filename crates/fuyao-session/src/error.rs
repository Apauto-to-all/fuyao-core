//! Session 错误类型

use thiserror::Error;

/// Session 错误
#[derive(Debug, Error)]
pub enum SessionError {
    /// IO 错误
    #[error("IO 错误: {0}")]
    IoError(#[from] std::io::Error),

    /// sqlx 错误
    #[error("sqlx 错误: {0}")]
    SqlxError(#[from] sqlx::Error),

    /// 无效状态（如 SessionManager 未初始化）
    #[error("无效状态: {0}")]
    InvalidState(String),

    /// 会话未找到
    #[error("会话未找到: {0}")]
    NotFound(String),
}
