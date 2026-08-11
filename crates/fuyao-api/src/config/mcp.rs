//! MCP 全局 fallback 配置
//!
//! MCP 运行的高频可调参数（超时、重连、熔断等）的默认值集合，
//! 通过 `[mcp]` 配置段覆盖。协议固定值不纳入配置。

use serde::Deserialize;

/// MCP 全局 fallback 配置
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpGlobalConfig {
    /// 默认工具调用超时（秒），原 `DEFAULT_TOOL_TIMEOUT=120`
    pub tool_timeout_secs: u64,
    /// 默认连接超时（秒），原 `DEFAULT_CONNECT_TIMEOUT=60`
    pub connect_timeout_secs: u64,
    /// 最大退避时间（秒），原 `MAX_BACKOFF_SECONDS=60`
    pub max_backoff_secs: u64,
    /// 最大重连次数，原 `MAX_RECONNECT_RETRIES=5`
    pub max_reconnect_retries: u32,
    /// 最大初始连接重试次数，原 `MAX_INITIAL_CONNECT_RETRIES=3`
    pub max_initial_connect_retries: u32,
    /// 熔断器触发阈值（连续失败次数），原 `CIRCUIT_BREAKER_THRESHOLD=3`
    pub circuit_breaker_threshold: u32,
    /// 熔断器冷却时间（秒），原 `CIRCUIT_BREAKER_COOLDOWN_SEC=60`
    pub circuit_breaker_cooldown_secs: u64,
    /// Session 恢复等待时间（秒），原 `SESSION_RECOVERY_WAIT_SEC=15`
    pub session_recovery_wait_secs: u64,
}

impl Default for McpGlobalConfig {
    fn default() -> Self {
        Self {
            tool_timeout_secs: 120,
            connect_timeout_secs: 60,
            max_backoff_secs: 60,
            max_reconnect_retries: 5,
            max_initial_connect_retries: 3,
            circuit_breaker_threshold: 3,
            circuit_breaker_cooldown_secs: 60,
            session_recovery_wait_secs: 15,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_global_config_defaults_match_hardcoded() {
        let c = McpGlobalConfig::default();
        assert_eq!(c.tool_timeout_secs, 120);
        assert_eq!(c.connect_timeout_secs, 60);
        assert_eq!(c.max_backoff_secs, 60);
        assert_eq!(c.max_reconnect_retries, 5);
        assert_eq!(c.max_initial_connect_retries, 3);
        assert_eq!(c.circuit_breaker_threshold, 3);
        assert_eq!(c.circuit_breaker_cooldown_secs, 60);
        assert_eq!(c.session_recovery_wait_secs, 15);
    }

    #[test]
    fn deserialize_mcp_global_partial() {
        let toml_str = r#"
[mcp]
tool_timeout_secs = 200
"#;
        #[derive(Deserialize)]
        struct Wrap {
            mcp: McpGlobalConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert_eq!(w.mcp.tool_timeout_secs, 200);
        // 缺省字段
        assert_eq!(w.mcp.connect_timeout_secs, 60);
        assert_eq!(w.mcp.circuit_breaker_threshold, 3);
    }
}
