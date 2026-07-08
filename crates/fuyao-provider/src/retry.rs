//! 重试策略 - 错误判断与退避计算

use crate::StreamError;
use fuyao_api::get_config;

/// 判断错误是否可重试
///
/// 可重试：RateLimit、Timeout、Connection、5xx ApiError
/// 不可重试：AuthError、StreamParseError、ContextOverflow、4xx ApiError
pub fn is_retryable(e: &StreamError) -> bool {
    match e {
        StreamError::RateLimit { .. } | StreamError::Timeout | StreamError::Connection(_) => true,
        StreamError::ApiError(msg) => {
            // 5xx 服务端错误视为可重试
            msg.contains("529")
                || msg.contains("500")
                || msg.contains("502")
                || msg.contains("503")
                || msg.contains("504")
        }
        // ContextOverflow 不重试，应触发压缩
        StreamError::ContextOverflow
        | StreamError::AuthError(_)
        | StreamError::StreamParseError(_) => false,
    }
}

/// 退避时长参数（从全局配置 `get_config().llm.retry` 读取）
///
/// 对应原硬编码常量：
/// - `initial_delay_ms`：原 `RETRY_INITIAL_DELAY=2000`
/// - `max_delay_ms`：无响应头场景上限，原 `RETRY_MAX_DELAY_NO_HEADERS=30000`
/// - `max_delay_with_headers_ms`：有响应头场景上限，原 `RETRY_MAX_DELAY=2_147_483_647`
struct BackoffParams {
    initial_delay_ms: u64,
    max_delay_ms: u64,
    max_delay_with_headers_ms: u64,
}

impl BackoffParams {
    fn from_config() -> Self {
        let r = &get_config().llm.retry;
        Self {
            initial_delay_ms: r.initial_delay_ms,
            max_delay_ms: r.max_delay_ms,
            max_delay_with_headers_ms: r.max_delay_with_headers_ms,
        }
    }
}

/// 计算退避时长（双分支策略）
///
/// 优先级：
/// 1. retry-after-ms 响应头
/// 2. retry-after 响应头
/// 3. 有响应头（RateLimit/5xx）→ 指数退避，上限 ~24.8天
/// 4. 无响应头（Timeout/Connection）→ 指数退避，上限 30s
pub fn backoff_duration(retry: u32, error: &StreamError) -> std::time::Duration {
    let p = BackoffParams::from_config();

    // 优先级 1: retry-after-ms 响应头
    if let StreamError::RateLimit {
        retry_after_ms: Some(ms),
        ..
    } = error
    {
        return std::time::Duration::from_millis((*ms).min(p.max_delay_with_headers_ms));
    }

    // 优先级 2: retry-after 响应头（秒 → 毫秒）
    if let StreamError::RateLimit {
        retry_after_secs: Some(secs),
        ..
    } = error
    {
        return std::time::Duration::from_millis((secs * 1000).min(p.max_delay_with_headers_ms));
    }

    // 优先级 3 & 4: 指数退避
    let base = p
        .initial_delay_ms
        .saturating_mul(2u64.saturating_pow(retry - 1));

    // 有响应头的错误（RateLimit、5xx ApiError）→ 上限 ~24.8天
    let has_headers = matches!(
        error,
        StreamError::RateLimit { .. } | StreamError::ApiError(_)
    );

    if has_headers {
        std::time::Duration::from_millis(base.min(p.max_delay_with_headers_ms))
    } else {
        // 无响应头的错误（Timeout、Connection）→ 上限 30s
        std::time::Duration::from_millis(base.min(p.max_delay_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_retryable_rate_limit() {
        assert!(is_retryable(&StreamError::RateLimit {
            retry_after_ms: None,
            retry_after_secs: None
        }));
    }

    #[test]
    fn is_retryable_timeout() {
        assert!(is_retryable(&StreamError::Timeout));
    }

    #[test]
    fn is_retryable_connection() {
        assert!(is_retryable(&StreamError::Connection(
            "refused".to_string()
        )));
    }

    #[test]
    fn is_retryable_5xx_api_error() {
        assert!(is_retryable(&StreamError::ApiError(
            "500 Internal Server Error".to_string()
        )));
        assert!(is_retryable(&StreamError::ApiError(
            "502 Bad Gateway".to_string()
        )));
        assert!(is_retryable(&StreamError::ApiError(
            "503 Service Unavailable".to_string()
        )));
        assert!(is_retryable(&StreamError::ApiError(
            "504 Gateway Timeout".to_string()
        )));
        assert!(is_retryable(&StreamError::ApiError(
            "529 Overloaded".to_string()
        )));
    }

    #[test]
    fn is_not_retryable_4xx_api_error() {
        assert!(!is_retryable(&StreamError::ApiError(
            "400 Bad Request".to_string()
        )));
        assert!(!is_retryable(&StreamError::ApiError(
            "401 Unauthorized".to_string()
        )));
        assert!(!is_retryable(&StreamError::ApiError(
            "404 Not Found".to_string()
        )));
    }

    #[test]
    fn is_not_retryable_auth_error() {
        assert!(!is_retryable(&StreamError::AuthError(
            "invalid key".to_string()
        )));
    }

    #[test]
    fn is_not_retryable_stream_parse_error() {
        assert!(!is_retryable(&StreamError::StreamParseError(
            "invalid json".to_string()
        )));
    }

    #[test]
    fn is_not_retryable_context_overflow() {
        assert!(!is_retryable(&StreamError::ContextOverflow));
    }

    #[test]
    fn backoff_duration_rate_limit_with_retry_after_ms() {
        // 优先级1: retry-after-ms 响应头
        let error = StreamError::RateLimit {
            retry_after_ms: Some(5000),
            retry_after_secs: None,
        };
        assert_eq!(
            backoff_duration(1, &error),
            std::time::Duration::from_millis(5000)
        );
    }

    #[test]
    fn backoff_duration_rate_limit_with_retry_after_secs() {
        // 优先级2: retry-after 响应头（秒 → 毫秒）
        let error = StreamError::RateLimit {
            retry_after_ms: None,
            retry_after_secs: Some(10),
        };
        assert_eq!(
            backoff_duration(1, &error),
            std::time::Duration::from_millis(10000)
        );
    }

    #[test]
    fn backoff_duration_rate_limit_exponential() {
        // 优先级3: 有响应头 → 指数退避，上限 ~24.8天
        let error = StreamError::RateLimit {
            retry_after_ms: None,
            retry_after_secs: None,
        };
        assert_eq!(
            backoff_duration(1, &error),
            std::time::Duration::from_millis(2000)
        );
        assert_eq!(
            backoff_duration(5, &error),
            std::time::Duration::from_millis(32000)
        );
        // 第20次: 2000 * 2^19 = 1048576000ms ≈ 12天，未超上限
        assert_eq!(
            backoff_duration(20, &error),
            std::time::Duration::from_millis(1048576000)
        );
        // 第31次: 2000 * 2^30 = 2147483648000ms，超过上限 → 封顶
        assert_eq!(
            backoff_duration(31, &error),
            std::time::Duration::from_millis(2_147_483_647)
        );
    }

    #[test]
    fn backoff_duration_timeout_capped_at_30s() {
        // 优先级4: 无响应头 → 上限 30s
        let error = StreamError::Timeout;
        assert_eq!(
            backoff_duration(1, &error),
            std::time::Duration::from_millis(2000)
        );
        assert_eq!(
            backoff_duration(5, &error),
            std::time::Duration::from_millis(30000) // 32s 被 30s 上限截断
        );
    }

    #[test]
    fn backoff_duration_connection_capped_at_30s() {
        let error = StreamError::Connection("refused".to_string());
        assert_eq!(
            backoff_duration(5, &error),
            std::time::Duration::from_millis(30000)
        );
    }

    #[test]
    fn backoff_duration_5xx_api_error_has_headers() {
        // 5xx ApiError 有响应头 → 上限 ~24.8天
        let error = StreamError::ApiError("503 Service Unavailable".to_string());
        assert_eq!(
            backoff_duration(5, &error),
            std::time::Duration::from_millis(32000)
        );
        // 第20次: 未超上限
        assert_eq!(
            backoff_duration(20, &error),
            std::time::Duration::from_millis(1048576000)
        );
        // 第31次: 超过上限 → 封顶
        assert_eq!(
            backoff_duration(31, &error),
            std::time::Duration::from_millis(2_147_483_647)
        );
    }
}
