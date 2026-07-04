//! MCP 错误恢复
//!
//! Auth Error 恢复 + Session Expired 恢复。
//! 检测特定错误模式，触发重连信号，等待 session 恢复后重试。

/// Auth 错误标记
const AUTH_ERROR_MARKERS: &[&str] = &[
    "401",
    "unauthorized",
    "invalid_token",
    "token_expired",
    "oauthflowerror",
];

/// Session 过期错误标记
const SESSION_EXPIRED_MARKERS: &[&str] = &[
    "invalid or expired session",
    "expired session",
    "session expired",
    "session not found",
    "unknown session",
];

/// 判断是否为 Auth 错误（字符串版本）
pub fn is_auth_error_str(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    AUTH_ERROR_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// 判断是否为 Session 过期错误（字符串版本）
pub fn is_session_expired_error_str(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    SESSION_EXPIRED_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_auth_error_str_detects_401() {
        assert!(is_auth_error_str("HTTP 401 Unauthorized"));
    }

    #[test]
    fn is_auth_error_str_detects_token_expired() {
        assert!(is_auth_error_str("token_expired for user"));
    }

    #[test]
    fn is_auth_error_str_ignores_normal_error() {
        assert!(!is_auth_error_str("connection refused"));
    }

    #[test]
    fn is_session_expired_error_str_detects_expired_session() {
        assert!(is_session_expired_error_str("session expired"));
    }

    #[test]
    fn is_session_expired_error_str_detects_session_not_found() {
        assert!(is_session_expired_error_str("session not found"));
    }

    #[test]
    fn is_session_expired_error_str_ignores_normal_error() {
        assert!(!is_session_expired_error_str("timeout"));
    }
}
