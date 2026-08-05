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

    /// 无效状态（如 SessionStore 未初始化）
    #[error("无效状态: {0}")]
    InvalidState(String),

    /// 会话未找到
    #[error("会话未找到: {0}")]
    NotFound(String),

    /// 无效的回退目标（目标消息不是 user 消息也不是 compaction 消息）
    #[error("无效的回退目标（只能回退到用户消息或压缩消息）: {0}")]
    InvalidRollbackTarget(String),
}
