//! LLM 调用层配置（HTTP 超时 / 重试退避）

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
    /// 可恢复错误的最大重试次数
    ///
    /// 默认 `u32::MAX`（无限重试）——可恢复错误的 session 级重试语义：
    /// 可恢复错误（RateLimit / Timeout / Connection / 5xx）说明供应商侧短时不可用，
    /// 引擎应持续重试直到恢复。用户嫌激进可在 TOML 里配小。
    ///
    /// 严重错误（Auth / 4xx / StreamParseError / ContextOverflow）不受此字段控制，
    /// 一律 0 次重试立即冒泡（由 `is_retryable` 判定）。
    pub max_retries: u32,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            initial_delay_ms: 2000,
            max_delay_ms: 30000,
            max_delay_with_headers_ms: 2_147_483_647,
            max_retries: u32::MAX,
        }
    }
}

/// LLM 调用层配置
///
/// 描述跨供应商的调用层行为（超时 / 退避），与 `[providers]`（模型定义清单）区分。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    /// 非流式请求的总超时（秒）：从连接到响应体读毕全程生效（标题生成等一次性短文本场景）。
    /// 流式对话不设总超时——长思考模型单次回复可达数十分钟，总死线会掐断健康流；
    /// 流停滞由 provider 层的 SSE 空闲超时防护
    pub request_timeout_secs: u64,
    /// HTTP 连接建立超时（秒），流式 / 非流式共用
    pub connect_timeout_secs: u64,
    /// 重试退避子段，对应 TOML `[llm.retry]`
    pub retry: RetryConfig,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            request_timeout_secs: 600,
            connect_timeout_secs: 10,
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
        // 默认无限重试（session 级重试语义）
        assert_eq!(c.max_retries, u32::MAX);
    }

    #[test]
    fn llm_config_defaults_match_hardcoded() {
        let c = LlmConfig::default();
        assert_eq!(c.request_timeout_secs, 600);
        assert_eq!(c.connect_timeout_secs, 10);
        assert_eq!(c.retry.initial_delay_ms, 2000);
        assert_eq!(c.retry.max_delay_ms, 30000);
        assert_eq!(c.retry.max_retries, u32::MAX);
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
        // max_retries 缺省也走 default（无限重试）
        assert_eq!(w.llm.retry.max_retries, u32::MAX);
    }

    /// 反序列化：max_retries 显式配置生效
    #[test]
    fn deserialize_llm_retry_max_retries_override() {
        let toml_str = r#"
[llm.retry]
max_retries = 5
"#;
        #[derive(Deserialize)]
        struct Wrap {
            #[serde(default)]
            llm: LlmConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert_eq!(w.llm.retry.max_retries, 5);
        // 其他字段缺省回退 default
        assert_eq!(w.llm.retry.initial_delay_ms, 2000);
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
        assert_eq!(w.llm.request_timeout_secs, 600);
        assert_eq!(w.llm.retry.initial_delay_ms, 2000);
    }
}
