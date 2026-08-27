//! Anthropic Messages 协议 HTTP 错误分类（纯函数，不依赖 HTTP）
//!
//! 把 HTTP 状态码 + 响应体文本分类为统一的 [`StreamError`]，供流式 / 非流式
//! 两条发送路径共用。与传输层解耦：输入是数值状态码 + 字符串响应体，
//! 输出是分类后的错误，可独立单测。
//!
//! Anthropic 错误体形态 `{"type":"error","error":{"type":"...","message":"..."}}`，
//! 分类只取状态码 + body 原文（原文进错误消息）。retry-after 在 HTTP 响应头，
//! 本函数签名不含头信息，限流退避走既有指数策略。

use crate::StreamError;

/// 根据 HTTP 状态码和响应体分类错误
///
/// - 401/403 → 认证失败（body 原文进消息）
/// - 413 → 上下文溢出（状态码本身已表明请求体过大，不依赖 body）
/// - 400 + 上下文超限关键词 → 上下文溢出
/// - 429 → 限流（无头信息可提取，退避走既有指数策略）
/// - 其余（含 500/502/503/504/529，529 为该协议过载错误码）→ 带状态码的
///   API 错误，可重试性由 [`StreamError::is_retryable`] 判定
pub fn classify_http_error(status_code: u16, body: &str) -> StreamError {
    match status_code {
        401 | 403 => StreamError::AuthError(body.to_string()),
        413 => StreamError::ContextOverflow,
        400 if has_context_overflow_keyword(body) => StreamError::ContextOverflow,
        429 => StreamError::RateLimit {
            retry_after_ms: None,
            retry_after_secs: None,
        },
        _ => StreamError::ApiError {
            status: Some(status_code),
            message: format!("HTTP {status_code}: {body}"),
        },
    }
}

/// body 是否含上下文超限关键词（`prompt is too long` / `context length`，大小写不敏感）
fn has_context_overflow_keyword(body: &str) -> bool {
    let lower = body.to_lowercase();
    lower.contains("prompt is too long") || lower.contains("context length")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 401/403 → AuthError，body 原文进消息
    #[test]
    fn classify_401_403_auth_error() {
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        for code in [401, 403] {
            match classify_http_error(code, body) {
                StreamError::AuthError(msg) => assert!(msg.contains("invalid x-api-key")),
                other => panic!("code={code} 期望 AuthError，实际 {other:?}"),
            }
        }
    }

    /// 413 → ContextOverflow（不依赖 body 关键词）
    #[test]
    fn classify_413_context_overflow() {
        assert!(matches!(
            classify_http_error(413, "payload too large"),
            StreamError::ContextOverflow
        ));
    }

    /// 400 + 上下文超限关键词 → ContextOverflow（两种关键词、大小写不敏感）
    #[test]
    fn classify_400_context_keyword_overflow() {
        for body in ["prompt is too long", "Context Length Exceeded"] {
            assert!(
                matches!(classify_http_error(400, body), StreamError::ContextOverflow),
                "body={body} 应归类为 ContextOverflow"
            );
        }
    }

    /// 400 不含关键词 → 普通 API 错误
    #[test]
    fn classify_400_without_keyword_is_api_error() {
        assert!(matches!(
            classify_http_error(400, "invalid request"),
            StreamError::ApiError {
                status: Some(400),
                ..
            }
        ));
    }

    /// 429 → RateLimit，无 retry-after 头信息可提取（退避走既有指数策略）
    #[test]
    fn classify_429_rate_limit_without_retry_after() {
        match classify_http_error(429, "rate limited") {
            StreamError::RateLimit {
                retry_after_ms,
                retry_after_secs,
            } => {
                assert_eq!(retry_after_ms, None);
                assert_eq!(retry_after_secs, None);
            }
            other => panic!("期望 RateLimit，实际 {other:?}"),
        }
    }

    /// 529（协议过载错误码）→ 带状态码的 ApiError，可重试
    #[test]
    fn classify_529_overloaded_api_error_retryable() {
        let err = classify_http_error(
            529,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        match &err {
            StreamError::ApiError { status, message } => {
                assert_eq!(*status, Some(529));
                assert!(message.contains("HTTP 529"));
                assert!(message.contains("Overloaded"));
            }
            other => panic!("期望 ApiError，实际 {other:?}"),
        }
        assert!(err.is_retryable());
    }

    /// 500 → ApiError，可重试
    #[test]
    fn classify_500_api_error_retryable() {
        let err = classify_http_error(500, "internal server error");
        assert!(matches!(
            &err,
            StreamError::ApiError {
                status: Some(500),
                ..
            }
        ));
        assert!(err.is_retryable());
    }

    /// 404 → ApiError，不可重试
    #[test]
    fn classify_404_api_error_not_retryable() {
        let err = classify_http_error(404, "not found");
        assert!(matches!(
            &err,
            StreamError::ApiError {
                status: Some(404),
                ..
            }
        ));
        assert!(!err.is_retryable());
    }
}
