//! WebFetch 工具处理函数
//!
//! 从 URL 抓取内容并转换为指定格式。
//!
//! 流程：
//! URL → 安全检查 → 检查缓存 → 缓存命中？
//!     ↓ 是
//! 返回缓存内容 → 应用分页 → 返回
//!     ↓ 否
//! HTTP 抓取 → 重定向处理 → 内容转换 → 缓存结果 → 应用分页 → 返回

use super::cache;
use super::converter::convert_content;
use super::pagination::{apply_pagination, validate_pagination};
use super::redirect::{get_redirect_url, is_same_domain_redirect};
use super::safety::check_url_safety;
use super::types::{WebFetchRedirect, WebFetchResult};
use crate::common;
use crate::config::{
    WEBFETCH_DEFAULT_TIMEOUT, WEBFETCH_MAX_DOWNLOAD_BYTES, WEBFETCH_MAX_TIMEOUT,
    WEBFETCH_USER_AGENT,
};
use serde_json::Value;
use std::time::Instant;

/// 验证并规范化超时时间
fn validate_timeout(timeout: Option<u64>) -> u64 {
    match timeout {
        None => WEBFETCH_DEFAULT_TIMEOUT,
        Some(t) => t.clamp(1, WEBFETCH_MAX_TIMEOUT),
    }
}

/// WebFetch 工具处理函数
pub async fn webfetch_handler(args: Value) -> String {
    let url = match args.get("url").and_then(|v| v.as_str()) {
        Some(u) => u.trim().to_string(),
        None => return common::tool_error("URL 不能为空"),
    };
    let output_format = args
        .get("output_format")
        .and_then(|v| v.as_str())
        .unwrap_or("markdown")
        .to_string();
    let timeout = validate_timeout(args.get("timeout").and_then(|v| v.as_u64()));
    let (offset, limit) = validate_pagination(
        args.get("offset")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize),
        args.get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize),
    );

    // 1. URL 验证
    if url.is_empty() {
        return common::tool_error("URL 不能为空");
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return common::tool_error("URL 必须以 http:// 或 https:// 开头");
    }

    // 2. 安全检查
    let safety_result = check_url_safety(&url).await;
    if !safety_result.safe {
        return common::tool_error(
            safety_result
                .message
                .as_deref()
                .unwrap_or("URL 不安全：私有网络地址或云元数据端点"),
        );
    }

    // 3. 检查缓存
    if let Some(cached) = cache::get_cached_content(&url, &output_format) {
        let pagination = apply_pagination(&cached.content, offset, limit);

        let result = WebFetchResult {
            url: url.clone(),
            content: pagination.content,
            output_format: output_format.clone(),
            content_bytes: 0,
            status: cached.status,
            content_type: cached.content_type,
            duration_ms: 0,
            offset,
            limit,
            total_length: pagination.total_length,
            has_more: pagination.has_more,
            next_offset: pagination.next_offset,
            redirect_url: None,
        };

        return common::tool_result(serde_json::to_value(result).unwrap_or_default());
    }

    // 4. HTTP 抓取（缓存未命中）
    let start = Instant::now();
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => return common::tool_error(&format!("创建 HTTP 客户端失败: {e}")),
    };

    let headers = build_request_headers();
    let mut current_url = url.clone();
    let mut response = match client
        .get(&current_url)
        .headers(headers.clone())
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            if e.is_timeout() {
                return common::tool_error(&format!("请求超时（{timeout}秒）"));
            }
            if e.is_connect() {
                return common::tool_error(&format!("网络错误: {e}"));
            }
            return common::tool_error(&format!("请求失败: {e}"));
        }
    };

    // 5. 处理重定向
    let status = response.status().as_u16();
    if matches!(status, 301 | 302 | 307 | 308) {
        let redirect_url = match get_redirect_url(&response) {
            Some(u) => u,
            None => return common::tool_error("重定向响应缺少 Location 头"),
        };

        if !is_same_domain_redirect(&current_url, &redirect_url) {
            let status_text = match status {
                301 => "Moved Permanently",
                302 => "Found",
                307 => "Temporary Redirect",
                308 => "Permanent Redirect",
                _ => "Redirect",
            };

            let result = WebFetchRedirect {
                original_url: current_url.clone(),
                redirect_url: redirect_url.clone(),
                status,
                message: format!(
                    "检测到跨域名重定向:\n原始 URL: {}\n目标 URL: {}\n状态: {status} {status_text}\n\n请使用新 URL 再次调用 webfetch",
                    args.get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&current_url),
                    redirect_url,
                ),
            };

            return common::tool_result(serde_json::to_value(result).unwrap_or_default());
        }

        // 同域名重定向：继续抓取
        response = match client.get(&redirect_url).headers(headers).send().await {
            Ok(r) => r,
            Err(e) => {
                if e.is_timeout() {
                    return common::tool_error(&format!("重定向请求超时（{timeout}秒）"));
                }
                return common::tool_error(&format!("重定向请求失败: {e}"));
            }
        };
        current_url = redirect_url;
    }

    // 6. 检查响应大小
    if let Some(content_length) = response.headers().get("content-length")
        && let Ok(len_str) = content_length.to_str()
        && let Ok(len) = len_str.parse::<usize>()
        && len > WEBFETCH_MAX_DOWNLOAD_BYTES
    {
        return common::tool_error(&format!(
            "响应过大（超过 {}MB 限制）",
            WEBFETCH_MAX_DOWNLOAD_BYTES / 1024 / 1024
        ));
    }

    let body_bytes = response.content_length().unwrap_or(0) as usize;
    if body_bytes > WEBFETCH_MAX_DOWNLOAD_BYTES {
        return common::tool_error(&format!(
            "响应过大（超过 {}MB 限制）",
            WEBFETCH_MAX_DOWNLOAD_BYTES / 1024 / 1024
        ));
    }

    // 7. 读取响应内容
    let final_status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let raw_content = match response.text().await {
        Ok(t) => t,
        Err(e) => return common::tool_error(&format!("读取响应内容失败: {e}")),
    };

    if raw_content.len() > WEBFETCH_MAX_DOWNLOAD_BYTES {
        return common::tool_error(&format!(
            "响应过大（超过 {}MB 限制）",
            WEBFETCH_MAX_DOWNLOAD_BYTES / 1024 / 1024
        ));
    }

    // 8. 内容转换
    let content = convert_content(&raw_content, &content_type, &output_format);

    // 9. 缓存转换后的内容
    cache::set_cached_content(
        &current_url,
        &output_format,
        content.clone(),
        content_type.clone(),
        final_status,
    );

    // 10. 应用分页
    let pagination = apply_pagination(&content, offset, limit);

    // 11. 构建结果
    let duration_ms = start.elapsed().as_millis() as u64;
    let content_bytes = pagination.content.len();

    let result = WebFetchResult {
        url: current_url,
        content: pagination.content,
        output_format,
        content_bytes,
        status: final_status,
        content_type,
        duration_ms,
        offset,
        limit,
        total_length: pagination.total_length,
        has_more: pagination.has_more,
        next_offset: pagination.next_offset,
        redirect_url: None,
    };

    common::tool_result(serde_json::to_value(result).unwrap_or_default())
}

/// 构建请求头
fn build_request_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static(WEBFETCH_USER_AGENT),
    );
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,text/plain;q=0.8,*/*;q=0.1",
        ),
    );
    headers.insert(
        reqwest::header::ACCEPT_LANGUAGE,
        reqwest::header::HeaderValue::from_static("en-US,en;q=0.9,zh-CN;q=0.8,zh;q=0.7"),
    );
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WEBFETCH_MAX_OUTPUT_CHARS;

    #[test]
    fn validate_timeout_defaults() {
        assert_eq!(validate_timeout(None), WEBFETCH_DEFAULT_TIMEOUT);
    }

    #[test]
    fn validate_timeout_clamps() {
        assert_eq!(validate_timeout(Some(0)), 1);
        assert_eq!(
            validate_timeout(Some(WEBFETCH_MAX_TIMEOUT + 100)),
            WEBFETCH_MAX_TIMEOUT
        );
    }

    #[test]
    fn validate_pagination_from_args() {
        let (offset, limit) = validate_pagination(None, None);
        assert_eq!(offset, 0);
        assert_eq!(limit, WEBFETCH_MAX_OUTPUT_CHARS);
    }

    #[tokio::test]
    async fn webfetch_handler_empty_url() {
        let result = webfetch_handler(serde_json::json!({ "url": "" })).await;
        assert!(result.contains("URL 不能为空"));
    }

    #[tokio::test]
    async fn webfetch_handler_invalid_scheme() {
        let result = webfetch_handler(serde_json::json!({ "url": "ftp://example.com" })).await;
        assert!(result.contains("http://") || result.contains("https://"));
    }
}
