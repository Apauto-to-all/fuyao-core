//! 重试策略 - 错误判断与退避计算

use crate::StreamError;
use fuyao_api::get_config;

/// 判断错误是否可重试
///
/// 可重试：RateLimit、Timeout、Connection、5xx ApiError
/// 不可重试：AuthError、StreamParseError、ContextOverflow、4xx ApiError、Cancelled、
///          无状态码的 ApiError（协议层异常，非 HTTP 错误）
pub fn is_retryable(e: &StreamError) -> bool {
    match e {
        StreamError::RateLimit { .. } | StreamError::Timeout | StreamError::Connection(_) => true,
        StreamError::ApiError {
            status: Some(code), ..
        } => {
            matches!(code, 500 | 502 | 503 | 504 | 529)
        }
        StreamError::ApiError { status: None, .. } => false,
        // ContextOverflow 不重试，应触发压缩
        // Cancelled 不重试（非错误，由 shutdown 流程触发，冒泡给上层走中断路径）
        StreamError::ContextOverflow
        | StreamError::AuthError(_)
        | StreamError::StreamParseError(_)
        | StreamError::Cancelled => false,
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

    // 有响应头的错误（RateLimit、带 HTTP 状态码的 ApiError）→ 上限 ~24.8天
    let has_headers = matches!(
        error,
        StreamError::RateLimit { .. }
            | StreamError::ApiError {
                status: Some(_),
                ..
            }
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
        assert!(is_retryable(&StreamError::ApiError {
            status: Some(500),
            message: "HTTP 500: Internal Server Error".to_string()
        }));
        assert!(is_retryable(&StreamError::ApiError {
            status: Some(502),
            message: "HTTP 502: Bad Gateway".to_string()
        }));
        assert!(is_retryable(&StreamError::ApiError {
            status: Some(503),
            message: "HTTP 503: Service Unavailable".to_string()
        }));
        assert!(is_retryable(&StreamError::ApiError {
            status: Some(504),
            message: "HTTP 504: Gateway Timeout".to_string()
        }));
        assert!(is_retryable(&StreamError::ApiError {
            status: Some(529),
            message: "HTTP 529: Overloaded".to_string()
        }));
    }

    #[test]
    fn is_not_retryable_4xx_api_error() {
        assert!(!is_retryable(&StreamError::ApiError {
            status: Some(400),
            message: "HTTP 400: Bad Request".to_string()
        }));
        assert!(!is_retryable(&StreamError::ApiError {
            status: Some(401),
            message: "HTTP 401: Unauthorized".to_string()
        }));
        assert!(!is_retryable(&StreamError::ApiError {
            status: Some(404),
            message: "HTTP 404: Not Found".to_string()
        }));
    }

    #[test]
    fn is_not_retryable_api_error_without_status() {
        // 协议层异常（无 HTTP 状态码）不可重试
        assert!(!is_retryable(&StreamError::ApiError {
            status: None,
            message: "响应中无 choice".to_string()
        }));
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
    fn is_not_retryable_cancelled() {
        // 取消不是错误（shutdown 触发），不应重试，应立即冒泡给上层走中断路径
        assert!(!is_retryable(&StreamError::Cancelled));
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
        // 5xx ApiError（带 HTTP 状态码）有响应头 → 上限 ~24.8天
        let error = StreamError::ApiError {
            status: Some(503),
            message: "HTTP 503: Service Unavailable".to_string(),
        };
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
