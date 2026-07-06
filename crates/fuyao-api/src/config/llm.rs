//! LLM 调用层配置
//!
//! 迁移自 `fuyao-provider/src/openai.rs`（HTTP 超时）、`retry.rs`（退避）、
//! `fuyao-core/src/llm/stream_session.rs`（ContextOverflow 等待）的硬编码。

use serde::Deserialize;

/// 重试退避配置（迁移自 `fuyao-provider/src/retry.rs` 常量）
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RetryConfig {
    /// 退避起始延迟（毫秒），原 `RETRY_INITIAL_DELAY=2000`
    pub initial_delay_ms: u64,
    /// 无响应头时的退避上限（毫秒），原 `RETRY_MAX_DELAY_NO_HEADERS=30000`
    pub max_delay_ms: u64,
    /// 有响应头时的退避上限（毫秒），原 `RETRY_MAX_DELAY=2_147_483_647`
    pub max_delay_with_headers_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            initial_delay_ms: 2000,
            max_delay_ms: 30000,
            max_delay_with_headers_ms: 2_147_483_647,
        }
    }
}

/// LLM 调用层配置
///
/// 描述跨供应商的调用层行为（超时 / 退避），与 `[providers]`（模型定义清单）区分。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    /// HTTP 请求超时（秒），原 `openai.rs:48` = 300
    pub request_timeout_secs: u64,
    /// HTTP 连接超时（秒），原 `openai.rs:49` = 10
    pub connect_timeout_secs: u64,
    /// ContextOverflow 后固定等待时长（秒），原 `stream_session.rs:160` = 2
    pub context_overflow_wait_secs: u64,
    /// 重试退避子段，对应 TOML `[llm.retry]`
    pub retry: RetryConfig,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            request_timeout_secs: 300,
            connect_timeout_secs: 10,
            context_overflow_wait_secs: 2,
            retry: RetryConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_config_defaults_match_hardcoded() {
        let c = RetryConfig::default();
        assert_eq!(c.initial_delay_ms, 2000);
        assert_eq!(c.max_delay_ms, 30000);
        assert_eq!(c.max_delay_with_headers_ms, 2_147_483_647);
    }

    #[test]
    fn llm_config_defaults_match_hardcoded() {
        let c = LlmConfig::default();
        assert_eq!(c.request_timeout_secs, 300);
        assert_eq!(c.connect_timeout_secs, 10);
        assert_eq!(c.context_overflow_wait_secs, 2);
        assert_eq!(c.retry.initial_delay_ms, 2000);
        assert_eq!(c.retry.max_delay_ms, 30000);
    }

    /// 反序列化：缺省字段走 Default
    #[test]
    fn deserialize_llm_partial() {
        let toml_str = r#"
[llm]
request_timeout_secs = 600
[llm.retry]
initial_delay_ms = 500
"#;
        #[derive(Deserialize)]
        struct Wrap {
            llm: LlmConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert_eq!(w.llm.request_timeout_secs, 600);
        // 缺省字段回退 default
        assert_eq!(w.llm.connect_timeout_secs, 10);
        assert_eq!(w.llm.retry.initial_delay_ms, 500);
        assert_eq!(w.llm.retry.max_delay_ms, 30000);
    }

    /// 完全缺省 `[llm]` 段时整体走 Default
    #[test]
    fn deserialize_llm_absent_uses_default() {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Wrap {
            llm: LlmConfig,
        }
        let w: Wrap = toml::from_str("").unwrap();
        assert_eq!(w.llm.request_timeout_secs, 300);
        assert_eq!(w.llm.retry.initial_delay_ms, 2000);
    }
}
