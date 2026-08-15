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
use super::types::{WebFetchArgs, WebFetchRedirect, WebFetchResult};
use crate::config::WEBFETCH_USER_AGENT;
use fuyao_api::{CancellationToken, ToolCallContext, ToolOutput, parse_args};
use serde_json::Value;
use std::sync::LazyLock;
use std::time::Instant;

/// 进程级共享 HTTP 客户端
///
/// 连接池与 TLS 会话跨调用复用，避免每次抓取重建客户端；禁用自动重定向
/// （重定向由本工具显式处理，跨域重定向要回报用户而非跟随），超时按请求
/// 粒度设置（RequestBuilder::timeout）。
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("webfetch 共享 HTTP 客户端构建失败")
});

/// 验证并规范化超时时间（默认/上限从全局配置 get_config().tools.limits 读取）
fn validate_timeout(timeout: Option<u64>) -> u64 {
    let limits = fuyao_api::get_config().tools.limits.clone();
    match timeout {
        None => limits.webfetch_default_timeout_secs,
        Some(t) => t.clamp(1, limits.webfetch_max_timeout_secs),
    }
}

/// WebFetch 工具处理函数
pub async fn webfetch_handler(
    args: Value,
    _ctx: ToolCallContext,
    _cancel: CancellationToken,
) -> ToolOutput {
    // 下载大小上限从全局配置读取
    let max_download_bytes = fuyao_api::get_config()
        .tools
        .limits
        .webfetch_max_download_bytes;
    let WebFetchArgs {
        url,
        output_format,
        timeout,
        offset,
        limit,
    } = match parse_args(args) {
        Ok(a) => a,
        Err(e) => return ToolOutput::Err(e),
    };
    let url = url.trim().to_string();
    let output_format = output_format.unwrap_or_else(|| "markdown".to_string());
    let timeout = validate_timeout(timeout);
    let (offset, limit) =
        validate_pagination(offset.map(|n| n as usize), limit.map(|n| n as usize));

    // 1. URL 验证
    if url.is_empty() {
        return ToolOutput::error("URL 不能为空");
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return ToolOutput::error("URL 必须以 http:// 或 https:// 开头");
    }

    // 2. 安全检查
    let safety_result = check_url_safety(&url).await;
    if !safety_result.safe {
        tracing::warn!(url = %url, "阻断 SSRF 危险 URL");
        return ToolOutput::error(
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

        return ToolOutput::ok(serde_json::to_value(result).unwrap_or_default());
    }

    // 4. HTTP 抓取（缓存未命中），共享客户端 + 请求粒度超时
    let start = Instant::now();
    let headers = build_request_headers();
    let mut current_url = url.clone();
    let mut response = match HTTP_CLIENT
        .get(&current_url)
        .headers(headers.clone())
        .timeout(std::time::Duration::from_secs(timeout))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            if e.is_timeout() {
                return ToolOutput::error(format!("请求超时（{timeout}秒）"));
            }
            if e.is_connect() {
                return ToolOutput::error(format!("网络错误: {e}"));
            }
            return ToolOutput::error(format!("请求失败: {e}"));
        }
    };

    // 5. 处理重定向
    let status = response.status().as_u16();
    if matches!(status, 301 | 302 | 307 | 308) {
        let redirect_url = match get_redirect_url(&response) {
            Some(u) => u,
            None => return ToolOutput::error("重定向响应缺少 Location 头"),
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
                    &url, redirect_url,
                ),
            };

            return ToolOutput::ok(serde_json::to_value(result).unwrap_or_default());
        }

        // 同域名重定向：继续抓取
        response = match HTTP_CLIENT
            .get(&redirect_url)
            .headers(headers)
            .timeout(std::time::Duration::from_secs(timeout))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                if e.is_timeout() {
                    return ToolOutput::error(format!("重定向请求超时（{timeout}秒）"));
                }
                return ToolOutput::error(format!("重定向请求失败: {e}"));
            }
        };
        current_url = redirect_url;
    }

    // 6. 检查响应大小（content-length 头；无头或不可解析按 0 放行，正文长度兜底）
    let body_bytes = response.content_length().unwrap_or(0) as usize;
    if body_bytes > max_download_bytes {
        return ToolOutput::error(format!(
            "响应过大（超过 {}MB 限制）",
            max_download_bytes / 1024 / 1024
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
        Err(e) => return ToolOutput::error(format!("读取响应内容失败: {e}")),
    };

    if raw_content.len() > max_download_bytes {
        return ToolOutput::error(format!(
            "响应过大（超过 {}MB 限制）",
            max_download_bytes / 1024 / 1024
        ));
    }

    // 8. 内容转换
    let content = convert_content(&raw_content, &content_type, &output_format);

    // 9. 应用分页（先于缓存：分页只读全文，缓存随后接管所有权）
    let pagination = apply_pagination(&content, offset, limit);

    // 10. 缓存转换后的内容（所有权移交缓存，省一次全文 clone）
    cache::set_cached_content(
        &current_url,
        &output_format,
        content,
        content_type.clone(),
        final_status,
    );

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

    ToolOutput::ok(serde_json::to_value(result).unwrap_or_default())
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

    #[test]
    fn validate_timeout_defaults() {
        let default = fuyao_api::get_config()
            .tools
            .limits
            .webfetch_default_timeout_secs;
        assert_eq!(validate_timeout(None), default);
    }

    #[test]
    fn validate_timeout_clamps() {
        let max = fuyao_api::get_config()
            .tools
            .limits
            .webfetch_max_timeout_secs;
        assert_eq!(validate_timeout(Some(0)), 1);
        assert_eq!(validate_timeout(Some(max + 100)), max);
    }

    #[test]
    fn validate_pagination_from_args() {
        let max_output = fuyao_api::get_config()
            .tools
            .limits
            .webfetch_max_output_chars;
        let (offset, limit) = validate_pagination(None, None);
        assert_eq!(offset, 0);
        assert_eq!(limit, max_output);
    }

    #[tokio::test]
    async fn webfetch_handler_empty_url() {
        let result = webfetch_handler(
            serde_json::json!({ "url": "" }),
            ToolCallContext::default(),
            CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("URL 不能为空"));
    }

    #[tokio::test]
    async fn webfetch_handler_invalid_scheme() {
        let result = webfetch_handler(
            serde_json::json!({ "url": "ftp://example.com" }),
            ToolCallContext::default(),
            CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("http://") || result.contains("https://"));
    }
}
