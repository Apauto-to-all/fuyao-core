//! 协议共享 HTTP 传输骨架
//!
//! wire 协议适配器（openai / anthropic）共用的传输层要素：构造期解析
//! （API Key / base_url / 客户端）、POST 请求装配、发送 + 错误分类、带空闲
//! 超时的流式读取、图片内容校验。协议差异项——默认端点、请求头组、HTTP
//! 错误分类函数——由各适配器以参数注入，传输行为单点维护。

use crate::provider::StreamError;
use futures_util::{Stream, StreamExt};
use fuyao_api::{AgentPaths, ImageContent};
use reqwest::Client;
use std::time::Duration;

/// SSE 流空闲超时：两次 chunk 到达之间的最长等待秒数
///
/// 流式对话不设请求总超时（长思考模型单次回复可达数十分钟，总死线会把
/// 健康的慢流拦腰掐断），连接停滞的防护完全由本空闲超时承担：每次取
/// chunk 用 `tokio::time::timeout` 单独包住，窗口内无数据即中断，映射为
/// 可重试的 [`StreamError::Timeout`]。
const SSE_IDLE_TIMEOUT_SECS: u64 = 90;

/// 协议支持的图片 MIME 白名单（仅图像：PNG / JPEG / WEBP / 非动画 GIF）
pub(crate) const SUPPORTED_IMAGE_MIMES: [&str; 4] =
    ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// 单图解码后字节上限（base64 长度 /4*3 估算解码字节数）
pub(crate) const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// HTTP 错误分类函数形态：状态码 + 响应体 → 统一流错误
///
/// 各协议的 body 关键词、retry-after 提取规则不同，分类逻辑由适配器注入。
pub(crate) type ClassifyHttpError = fn(u16, &str) -> StreamError;

/// Provider HTTP 构造要素
pub(crate) struct HttpParts {
    /// 鉴权用 API Key
    pub(crate) api_key: String,
    /// 请求端点根（协议默认端点兜底后）
    pub(crate) base_url: String,
    /// HTTP 客户端（复用连接池）
    pub(crate) client: Client,
}

/// 解析 Provider HTTP 构造要素
///
/// 注册表解析 API Key（未解析到 WARN 返回 None）→ 注册表解析 base_url
/// （缺省回退 `default_base_url`）→ 按全局配置 `llm.connect_timeout_secs`
/// 构建客户端（构建失败 WARN 返回 None）。供应商级容错：单个供应商要素
/// 缺失即跳过，不拖垮其余供应商。
pub(crate) fn resolve_http_parts(
    provider_id: &str,
    agent_paths: &AgentPaths,
    default_base_url: &str,
) -> Option<HttpParts> {
    let api_key = match crate::resolver::resolve_api_key(provider_id, agent_paths) {
        Some(key) => key,
        None => {
            tracing::warn!(provider = %provider_id, "Provider 创建失败：未解析到 API Key");
            return None;
        }
    };
    let base_url = crate::resolver::get_base_url(provider_id, agent_paths)
        .unwrap_or_else(|| default_base_url.to_string());

    // 客户端只设连接建立超时，不设请求总超时：流式 body 时长无上界（长思考
    // 模型单次回复可达数十分钟），总死线会把健康的慢流拦腰掐断。流停滞防护
    // 由 [`next_chunk`] 的空闲超时承担；非流式请求在各自调用点设请求级总超时。
    let llm = fuyao_api::get_config().llm.clone();
    let client = match Client::builder()
        .connect_timeout(Duration::from_secs(llm.connect_timeout_secs))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(provider = %provider_id, cause = %e, "Provider 创建失败：HTTP 客户端构建失败");
            return None;
        }
    };

    Some(HttpParts {
        api_key,
        base_url,
        client,
    })
}

/// 非流式请求的总超时时长（读全局配置 `llm.request_timeout_secs`）
///
/// 流式对话不设总超时（见 [`resolve_http_parts`] 的客户端构建说明）；非流式
/// `chat()`（标题生成等一次性短文本场景）以本值设请求级总超时，覆盖从连接
/// 到响应体读毕的全程，防止响应永不返回时调用方永久挂起。
pub(crate) fn request_timeout() -> Duration {
    Duration::from_secs(fuyao_api::get_config().llm.request_timeout_secs)
}

/// 构造 POST 请求构建器：目标 URL + 注入请求头组 + Content-Type + JSON body（未发送）
///
/// 流式 / 非流式两条发送路径共用的装配入口；请求头组是协议差异项（如
/// Bearer vs x-api-key + 版本头），由适配器注入。返回 owned
/// `RequestBuilder`——可在 `async_stream` 块外构造、块内再 send，无需跨越
/// yield 持有 `&self`。
pub(crate) fn post_json(
    client: &Client,
    url: &str,
    headers: &[(&str, &str)],
    body: &serde_json::Value,
) -> reqwest::RequestBuilder {
    let mut builder = client.post(url);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder
        .header("Content-Type", "application/json")
        .json(body)
}

/// 发送请求 + 错误分类（url / 请求头装配之后的完整发送链路）
///
/// 执行 send → 网络错误映射（timeout / connection）→ HTTP 状态码校验 →
/// 错误分类（分类函数由适配器注入）。流式 / 非流式两条发送路径共用，
/// 集中「send + timeout 映射 + status 校验 + classify」逻辑。
pub(crate) async fn execute(
    builder: reqwest::RequestBuilder,
    classify: ClassifyHttpError,
) -> Result<reqwest::Response, StreamError> {
    let response = builder.send().await.map_err(|e| {
        if e.is_timeout() {
            StreamError::Timeout
        } else {
            StreamError::Connection(e.to_string())
        }
    })?;

    let status = response.status();
    if !status.is_success() {
        let status_code = status.as_u16();
        let body_text = response.text().await.unwrap_or_default();
        return Err(classify(status_code, &body_text));
    }

    Ok(response)
}

/// 带空闲超时读取字节流下一块
///
/// `Ok(None)` 表示流正常结束（服务端关闭连接）；窗口内无数据映射为可重试的
/// [`StreamError::Timeout`]（静默连接场景见 [`SSE_IDLE_TIMEOUT_SECS`] 说明）。
pub(crate) async fn next_chunk<S>(stream: &mut S) -> Result<Option<S::Item>, StreamError>
where
    S: Stream + Unpin,
{
    match tokio::time::timeout(Duration::from_secs(SSE_IDLE_TIMEOUT_SECS), stream.next()).await {
        Ok(item) => Ok(item),
        Err(_elapsed) => Err(StreamError::Timeout),
    }
}

/// 校验并过滤图片：MIME 白名单 + 协议字节上限，不合规的丢弃并告警
///
/// 校验失败只丢单张图、不中断整条消息——图像是辅助信息，文本对话照常。
pub(crate) fn filter_valid_images(images: &[ImageContent]) -> Vec<&ImageContent> {
    images
        .iter()
        .filter(|img| {
            if !SUPPORTED_IMAGE_MIMES.contains(&img.mime_type.as_str()) {
                tracing::warn!(mime_type = %img.mime_type, "图片 MIME 不在协议白名单，已丢弃");
                return false;
            }
            if img.data.len() / 4 * 3 > MAX_IMAGE_BYTES {
                tracing::warn!(
                    mime_type = %img.mime_type,
                    base64_len = img.data.len(),
                    "图片超过 20MB 协议上限，已丢弃"
                );
                return false;
            }
            true
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{agent_paths_cache_key, clear_cache, register_provider};
    use fuyao_api::{Provider as ProviderConfig, ProviderOptions};

    /// 注入用分类函数：状态码原样进 ApiError，用于验证注入项被调用
    fn echo_classify(status_code: u16, _body: &str) -> StreamError {
        StreamError::ApiError {
            status: Some(status_code),
            message: "已注入".to_string(),
        }
    }

    /// 注册一个带 API Key 的供应商，返回其唯一化 AgentPaths
    fn registered_paths(test_name: &str, base_url: Option<&str>) -> AgentPaths {
        let paths = AgentPaths {
            agent_id: Some(format!("global/{test_name}")),
            workspace: None,
            ..Default::default()
        };
        let key = agent_paths_cache_key(&paths);
        let provider = ProviderConfig {
            name: test_name.to_string(),
            api_protocol: fuyao_api::ApiProtocol::OpenaiCompletions,
            models: std::collections::HashMap::new(),
            options: ProviderOptions {
                api_key: Some("test-key".to_string()),
                base_url: base_url.map(str::to_string),
            },
            api_key_env_vars: Vec::new(),
        };
        register_provider("vendor", provider, &key);
        paths
    }

    /// base_url 未配置时回退注入的协议默认端点
    #[test]
    fn resolve_http_parts_falls_back_to_default_base_url() {
        let paths = registered_paths("http_parts_default", None);
        let parts = resolve_http_parts("vendor", &paths, "https://default.example").unwrap();
        assert_eq!(parts.api_key, "test-key");
        assert_eq!(parts.base_url, "https://default.example");
        clear_cache(&paths);
    }

    /// 注册表配置了 base_url 时优先于默认端点
    #[test]
    fn resolve_http_parts_prefers_configured_base_url() {
        let paths = registered_paths("http_parts_configured", Some("https://custom.example/v1"));
        let parts = resolve_http_parts("vendor", &paths, "https://default.example").unwrap();
        assert_eq!(parts.base_url, "https://custom.example/v1");
        clear_cache(&paths);
    }

    /// API Key 未解析到时返回 None
    #[test]
    fn resolve_http_parts_none_without_api_key() {
        let paths = AgentPaths {
            agent_id: Some("global/http_parts_nokey".to_string()),
            workspace: None,
            ..Default::default()
        };
        assert!(resolve_http_parts("ghost", &paths, "https://default.example").is_none());
    }

    /// 装配注入：URL、请求头组、Content-Type 逐项落位
    #[test]
    fn post_json_carries_url_headers_and_content_type() {
        let request = post_json(
            &Client::new(),
            "https://api.test.com/v1/messages",
            &[
                ("x-api-key", "test-key"),
                ("anthropic-version", "2023-06-01"),
            ],
            &serde_json::json!({"model": "m"}),
        )
        .build()
        .expect("请求构建失败");

        assert_eq!(request.url().as_str(), "https://api.test.com/v1/messages");
        let headers = request.headers();
        assert_eq!(headers.get("x-api-key").unwrap(), "test-key");
        assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
    }

    /// 发送成功：2xx 响应原样返回
    #[tokio::test]
    async fn execute_returns_response_on_success() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/chat")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;

        let url = format!("{}/chat", server.url());
        let builder = post_json(&Client::new(), &url, &[], &serde_json::json!({}));
        let response = execute(builder, echo_classify)
            .await
            .expect("2xx 应返回响应");
        assert_eq!(response.status().as_u16(), 200);
    }

    /// 非成功状态码走注入的分类函数（注入项原样决定错误内容）
    #[tokio::test]
    async fn execute_delegates_error_classification_to_injected_fn() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/chat")
            .with_status(503)
            .with_body("boom")
            .create_async()
            .await;

        let url = format!("{}/chat", server.url());
        let builder = post_json(&Client::new(), &url, &[], &serde_json::json!({}));
        match execute(builder, echo_classify).await {
            Err(StreamError::ApiError { status, message }) => {
                assert_eq!(status, Some(503));
                assert_eq!(message, "已注入");
            }
            other => panic!("期望注入分类的 ApiError，实际 {other:?}"),
        }
    }

    /// 连接失败（目标端口无监听）映射为 Connection
    #[tokio::test]
    async fn execute_maps_connection_refused() {
        // 先占端口再释放：保证目标端口确定无监听
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let builder = post_json(
            &Client::new(),
            &format!("http://{addr}"),
            &[],
            &serde_json::json!({}),
        );
        match execute(builder, echo_classify).await {
            Err(StreamError::Connection(_)) => {}
            other => panic!("期望 Connection，实际 {other:?}"),
        }
    }

    /// 窗口内无数据：静默流映射为可重试 Timeout
    #[tokio::test(start_paused = true)]
    async fn next_chunk_times_out_on_silent_stream() {
        let mut silent = futures_util::stream::pending::<Result<u8, u8>>();
        match next_chunk(&mut silent).await {
            Err(StreamError::Timeout) => {}
            other => panic!("期望 Timeout，实际 {other:?}"),
        }
    }

    /// 正常产出与流结束两条路径原样透传
    #[tokio::test(start_paused = true)]
    async fn next_chunk_passes_through_item_and_end() {
        let mut stream = futures_util::stream::iter(vec![Ok::<u8, u8>(7)]);
        assert_eq!(next_chunk(&mut stream).await.unwrap(), Some(Ok(7)));
        assert_eq!(next_chunk(&mut stream).await.unwrap(), None);
    }

    /// MIME 白名单内的图片保留、白名单外丢弃
    #[test]
    fn filter_valid_images_keeps_whitelisted_mimes_only() {
        let images = vec![
            ImageContent {
                mime_type: "image/png".to_string(),
                data: "aGVsbG8=".to_string(),
            },
            ImageContent {
                mime_type: "application/pdf".to_string(),
                data: "eA==".to_string(),
            },
        ];
        let valid = filter_valid_images(&images);
        assert_eq!(valid.len(), 1, "pdf 应被丢弃");
        assert_eq!(valid[0].mime_type, "image/png");
    }
}
