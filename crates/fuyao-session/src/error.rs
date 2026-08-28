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

    /// 会话未找到
    #[error("会话未找到: {0}")]
    NotFound(String),

    /// 无效的切割目标（目标消息不是 user 消息也不是 compaction 消息，
    /// 回退与派生共用此判定）
    #[error("无效的切割目标（只能切到用户消息或压缩消息）: {0}")]
    InvalidCutTarget(String),
}

impl SessionError {
    /// 判别是否为主键 / 唯一约束冲突
    ///
    /// 仅用于 session id 随机生成碰撞时的精准重试触发：其它错误（磁盘满、连接断、
    /// schema 错误等）重试无意义，不应重试。封装 sqlx 的 `is_unique_violation`，
    /// 使调用方不必直接接触 sqlx 内部错误结构。
    pub fn is_primary_key_conflict(&self) -> bool {
        match self {
            SessionError::SqlxError(sqlx::Error::Database(db)) => db.is_unique_violation(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_database_errors_are_not_conflicts() {
        // 非 Database 类 sqlx 错误（如连接断）不算主键冲突，不应触发重试
        let io_err = SessionError::IoError(std::io::Error::other("磁盘故障"));
        assert!(!io_err.is_primary_key_conflict());

        let not_found = SessionError::NotFound("xxx".into());
        assert!(!not_found.is_primary_key_conflict());
    }
}
