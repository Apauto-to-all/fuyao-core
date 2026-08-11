//! HTTP 错误分类（纯函数，不依赖 HTTP）
//!
//! 把 HTTP 状态码 + 响应体文本分类为统一的 [`StreamError`]，供流式 / 非流式
//! 两条发送路径共用。与传输层解耦：输入是数值状态码 + 字符串响应体，
//! 输出是分类后的错误，可独立单测。

use crate::StreamError;

/// 根据 HTTP 状态码和响应体分类错误
pub(crate) fn classify_http_error(status_code: u16, body: &str) -> StreamError {
    match status_code {
        401 | 403 => StreamError::AuthError(body.to_string()),
        // 413 Payload Too Large：请求体超过供应商上限，按上下文溢出处理
        // 引擎不对该错误自动兜底（如自动压缩），交由上层应用识别后自行决策
        // （提示用户、切换模型、或允许用户主动压缩）
        413 => StreamError::ContextOverflow,
        429 => {
            let retry_after_ms = extract_retry_after_ms(body);
            let retry_after_secs = extract_retry_after_secs(body);
            StreamError::RateLimit {
                retry_after_ms,
                retry_after_secs,
            }
        }
        _ => {
            // 检测上下文溢出（覆盖 400 + body 含 OpenAI 风格关键词的场景）
            if body.contains("context_length_exceeded") || body.contains("maximum context length") {
                return StreamError::ContextOverflow;
            }
            StreamError::ApiError {
                status: Some(status_code),
                message: format!("HTTP {status_code}: {body}"),
            }
        }
    }
}

/// 从错误消息中提取 retry-after-ms 值
///
/// 支持两种格式：`retry-after-ms:5000` 或 `retry-after-ms: 5000`
fn extract_retry_after_ms(msg: &str) -> Option<u64> {
    let lower = msg.to_lowercase();
    for part in lower.split_whitespace() {
        if let Some(val) = part.strip_prefix("retry-after-ms:")
            && !val.is_empty()
            && let Ok(ms) = val.trim_end_matches(',').parse()
        {
            return Some(ms);
        }
    }
    // 尝试匹配 "retry-after-ms: <value>" 格式（冒号后有空格，value 单独一个 token）
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    for i in 0..tokens.len().saturating_sub(1) {
        if tokens[i] == "retry-after-ms:"
            && let Ok(ms) = tokens[i + 1].trim_end_matches(',').parse()
        {
            return Some(ms);
        }
    }
    None
}

/// 从错误消息中提取 retry-after 秒数
///
/// 支持两种格式：`retry-after:10` 或 `retry-after: 10`
fn extract_retry_after_secs(msg: &str) -> Option<u64> {
    let lower = msg.to_lowercase();
    for part in lower.split_whitespace() {
        if let Some(val) = part.strip_prefix("retry-after:")
            && !val.is_empty()
            && let Ok(secs) = val.trim_end_matches(',').parse()
        {
            return Some(secs);
        }
    }
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    for i in 0..tokens.len().saturating_sub(1) {
        if tokens[i] == "retry-after:"
            && let Ok(secs) = tokens[i + 1].trim_end_matches(',').parse()
        {
            return Some(secs);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_http_error_401() {
        let err = classify_http_error(401, "unauthorized");
        assert!(matches!(err, StreamError::AuthError(_)));
    }

    #[test]
    fn classify_http_error_429() {
        let err = classify_http_error(429, "rate limited");
        assert!(matches!(err, StreamError::RateLimit { .. }));
    }

    #[test]
    fn classify_http_error_context_overflow() {
        let err = classify_http_error(400, "context_length_exceeded: too many tokens");
        assert!(matches!(err, StreamError::ContextOverflow));
    }

    #[test]
    fn classify_http_error_413_payload_too_large() {
        // 413 Payload Too Large：HTTP 状态码已明确表示请求体过大，
        // 不依赖 body 关键词匹配，直接归类为 ContextOverflow
        let err = classify_http_error(413, "payload too large");
        assert!(matches!(err, StreamError::ContextOverflow));
    }

    #[test]
    fn classify_http_error_413_with_unexpected_body_still_overflow() {
        // 413 即便 body 是空字符串或非典型格式，仍是上下文溢出
        let err = classify_http_error(413, "");
        assert!(matches!(err, StreamError::ContextOverflow));
    }

    #[test]
    fn classify_http_error_generic() {
        let err = classify_http_error(500, "internal server error");
        assert!(matches!(
            err,
            StreamError::ApiError {
                status: Some(500),
                ..
            }
        ));
    }

    #[test]
    fn retry_after_ms_extraction() {
        assert_eq!(
            extract_retry_after_ms("retry-after-ms: 5000, other"),
            Some(5000)
        );
        assert_eq!(extract_retry_after_ms("no retry info"), None);
    }

    #[test]
    fn retry_after_secs_extraction() {
        assert_eq!(extract_retry_after_secs("retry-after: 10, other"), Some(10));
        assert_eq!(extract_retry_after_secs("no info"), None);
    }
}
