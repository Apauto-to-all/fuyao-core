//! MCP 常量定义
//!
//! 超时、重连、熔断器、协议版本等常量。

/// 默认工具调用超时（秒）
pub const DEFAULT_TOOL_TIMEOUT: u64 = 120;

/// 默认连接超时（秒）
pub const DEFAULT_CONNECT_TIMEOUT: u64 = 60;

/// 最大退避时间（秒）
pub const MAX_BACKOFF_SECONDS: u64 = 60;

/// 最大重连次数
pub const MAX_RECONNECT_RETRIES: u32 = 5;

/// 最大初始连接重试次数
pub const MAX_INITIAL_CONNECT_RETRIES: u32 = 3;

/// 熔断器触发阈值（连续失败次数）
pub const CIRCUIT_BREAKER_THRESHOLD: u32 = 3;

/// 熔断器冷却时间（秒）
pub const CIRCUIT_BREAKER_COOLDOWN_SEC: u64 = 60;

/// 最新 MCP 协议版本
pub const LATEST_PROTOCOL_VERSION: &str = "2025-03-26";

/// Session 恢复等待时间（秒）
pub const SESSION_RECOVERY_WAIT_SEC: u64 = 15;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_timeouts_are_reasonable() {
        assert_eq!(DEFAULT_TOOL_TIMEOUT, 120);
        assert_eq!(DEFAULT_CONNECT_TIMEOUT, 60);
    }

    #[test]
    fn retry_limits_are_set() {
        assert!(MAX_RECONNECT_RETRIES > 0);
        assert!(MAX_INITIAL_CONNECT_RETRIES > 0);
    }

    #[test]
    fn circuit_breaker_config_is_valid() {
        assert!(CIRCUIT_BREAKER_THRESHOLD > 0);
        assert!(CIRCUIT_BREAKER_COOLDOWN_SEC > 0);
    }

    #[test]
    fn protocol_version_is_not_empty() {
        assert!(!LATEST_PROTOCOL_VERSION.is_empty());
    }
}
