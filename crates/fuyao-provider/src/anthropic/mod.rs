//! Anthropic Messages 协议 Provider 实现（薄 HTTP adapter）
//!
//! 基于 reqwest 直接请求 `/v1/messages`。本模块只承担 HTTP 交互——
//! 请求体编码（[`request`]）、流式线解码（[`sse`]）、HTTP 错误分类（[`classify`]）、
//! 非流式响应解析（[`completion`]）均已拆为独立纯函数模块，各自可单测。
//!
//! 协议交互要素：
//! - 鉴权头 `x-api-key` + 固定版本头 `anthropic-version: 2023-06-01`
//! - 流式/非流式对话共用一套请求装配与发送链
//! - 流式 body 逐块消费，每次取 chunk 限定空闲超时窗口

mod classify;
mod completion;
mod request;
mod sse;

use crate::provider::{
    BoxStream, ChatRequest, ChatResponse, Provider, StreamError, StreamEvent, StreamOptions,
};
use crate::sse::LineAssembler;
use async_trait::async_trait;
use futures_util::StreamExt;
use fuyao_api::AgentPaths;
use reqwest::Client;
use std::time::Duration;

/// 协议要求的固定版本头取值
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// SSE 流空闲超时：两次 chunk 到达之间的最长等待秒数
///
/// reqwest Client 的整体 `.timeout()` 对逐块消费的流式 body 不可靠——代理 /
/// 负载均衡器掐断连接后 `bytes_stream.next().await` 可能永久挂起，静默连接
/// 会让整个轮次卡死。每次取 chunk 用 `tokio::time::timeout` 单独包住，
/// 窗口内无数据即中断，映射为可重试的 [`StreamError::Timeout`]。
const SSE_IDLE_TIMEOUT_SECS: u64 = 90;

/// Anthropic Messages 协议 Provider
///
/// 通过 reqwest 直接发送 HTTP 请求到 Anthropic 兼容 API。
/// 本结构只持有 HTTP 交互所需的客户端与鉴权信息；请求构造、流解码、错误分类
/// 委托给同目录下的纯函数模块。
pub struct AnthropicProvider {
    /// HTTP 客户端（复用连接池）
    client: Client,
    /// messages 完整请求 URL（构造时定形，Provider 生命周期内不变）
    messages_url: String,
    /// API Key（`x-api-key` 请求头的取值来源）
    api_key: String,
}

impl AnthropicProvider {
    /// 构造不变的完整 messages 端点
    ///
    /// 去尾斜杠；base_url 已含 `/v1/messages` 完整路径则直用；以 `/v1` 结尾
    /// （只给到版本根）则补 `/messages`；其余拼 `/v1/messages`。
    fn build_messages_url(base_url: &str) -> String {
        let base = base_url.trim_end_matches('/');
        if base.ends_with("/v1/messages") {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        }
    }

    /// 从 provider_id 和 agent_paths 创建
    ///
    /// 从注册表解析 api_key 和 base_url（默认官方端点），构建 reqwest Client。
    /// HTTP 超时从全局配置 `get_config().llm` 读取。
    pub fn new(provider_id: &str, agent_paths: &AgentPaths) -> Option<Self> {
        let api_key = match crate::resolver::resolve_api_key(provider_id, agent_paths) {
            Some(key) => key,
            None => {
                tracing::warn!(provider = %provider_id, "Provider 创建失败：未解析到 API Key");
                return None;
            }
        };
        let base_url = crate::resolver::get_base_url(provider_id, agent_paths)
            .unwrap_or_else(|| "https://api.anthropic.com".to_string());

        let llm = fuyao_api::get_config().llm.clone();
        let client = match Client::builder()
            .timeout(Duration::from_secs(llm.request_timeout_secs))
            .connect_timeout(Duration::from_secs(llm.connect_timeout_secs))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(provider = %provider_id, cause = %e, "Provider 创建失败：HTTP 客户端构建失败");
                return None;
            }
        };

        Some(Self::from_parts(api_key, base_url, client))
    }

    /// 从已有配置创建（用于测试）
    pub fn from_parts(api_key: String, base_url: String, client: Client) -> Self {
        let messages_url = Self::build_messages_url(&base_url);
        Self {
            client,
            messages_url,
            api_key,
        }
    }

    /// 构造已带 url + 鉴权头 + 版本头 + content-type 的 POST 请求构建器（未发送）
    ///
    /// 非流式 [`Provider::chat`] 与流式 [`Provider::stream_chat`] 两条发送路径共用，
    /// 避免请求头 / url 装配逻辑两处复制。返回 owned `RequestBuilder`——可在
    /// `async_stream` 块**外**构造、块内再 send，无需跨越 yield 持有 `&self`。
    fn post_builder(&self, body: &serde_json::Value) -> reqwest::RequestBuilder {
        self.client
            .post(&self.messages_url)
            .header("x-api-key", self.api_key.as_str())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("Content-Type", "application/json")
            .json(body)
    }

    /// 发送请求 + 错误分类（url / 请求头装配之后的完整发送链路）
    ///
    /// 接收 [`Self::post_builder`] 产出的构建器，执行 send → 网络错误映射（timeout /
    /// connection）→ HTTP 状态码校验 → [`classify::classify_http_error`]。两条发送路径
    /// 共用此方法，集中「send + timeout 映射 + status 校验 + classify」逻辑。
    async fn execute(builder: reqwest::RequestBuilder) -> Result<reqwest::Response, StreamError> {
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
            return Err(classify::classify_http_error(status_code, &body_text));
        }

        Ok(response)
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn stream_chat(
        &self,
        request: ChatRequest,
        model: &str,
        options: StreamOptions,
    ) -> BoxStream<Result<StreamEvent, StreamError>> {
        let body = request::build_request_body(request, model, &options, true);
        // 在 stream 块外构造请求构建器（owned，不持有 &self 跨越 yield）
        let request_builder = self.post_builder(&body);

        let stream = async_stream::stream! {
            // 发送 + 网络错误映射 + 状态码校验 + 错误分类（与 chat() 共用 execute）
            let response = match Self::execute(request_builder).await {
                Ok(r) => r,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };

            // 逐 chunk 消费 SSE 字节流
            let mut bytes_stream = response.bytes_stream();
            // SSE 行组装器：字节上按 \n 切行，跨 chunk 的半行 / 半个多字节
            // 字符滞留其内部缓冲等续包，行内非法 UTF-8 以替换字符顶替
            let mut assembler = LineAssembler::new();
            // 有状态线解码器：usage 分桶、content block 序号 → 工具调用序号映射
            // 均在其内部累积
            let mut decoder = sse::AnthropicStreamDecoder::new();

            loop {
                // 每次取 chunk 限定空闲超时窗口，静默连接不再永久挂起
                let item = match tokio::time::timeout(
                    Duration::from_secs(SSE_IDLE_TIMEOUT_SECS),
                    bytes_stream.next(),
                )
                .await
                {
                    Ok(item) => item,
                    Err(_elapsed) => {
                        yield Err(StreamError::Timeout);
                        return;
                    }
                };

                let Some(item) = item else {
                    // 字节流结束（服务端关闭连接）
                    break;
                };
                let bytes = match item {
                    Ok(b) => b,
                    Err(e) => {
                        yield Err(StreamError::StreamParseError(format!(
                            "字节流读取失败: {e}"
                        )));
                        return;
                    }
                };

                // 切出本 chunk 内所有完整行并逐行线解码
                for line in assembler.push(&bytes) {
                    match decoder.feed_line(&line) {
                        Ok(events) => {
                            for event in events {
                                yield Ok(event);
                            }
                        }
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    }
                }
            }
        };

        Box::pin(stream)
    }

    async fn chat(
        &self,
        request: ChatRequest,
        model: &str,
        options: StreamOptions,
    ) -> Result<ChatResponse, StreamError> {
        let started = std::time::Instant::now();
        let body = request::build_request_body(request, model, &options, false);
        let response = Self::execute(self.post_builder(&body)).await?;

        let response_text = response
            .text()
            .await
            .map_err(|e| StreamError::StreamParseError(format!("读取响应体失败: {e}")))?;

        let chat_response = completion::parse_completion(&response_text)?;

        tracing::info!(
            model = %model,
            elapsed_ms = started.elapsed().as_millis() as u64,
            tokens_in = chat_response.usage.prompt_tokens,
            tokens_out = chat_response.usage.completion_tokens,
            "LLM 请求完成"
        );

        Ok(chat_response)
    }
}

// URL 拼装 / 请求头拼装等保留在 adapter 上的纯字符串操作；其余职责的测试见各子模块
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_url_appends_path_to_official_base() {
        let provider = test_provider_with_base("https://api.anthropic.com");
        assert_eq!(
            provider.messages_url,
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn messages_url_appends_path_to_custom_base() {
        let provider = test_provider_with_base("https://open.bigmodel.cn/api/anthropic");
        assert_eq!(
            provider.messages_url,
            "https://open.bigmodel.cn/api/anthropic/v1/messages"
        );
    }

    #[test]
    fn messages_url_strips_trailing_slash() {
        let provider = test_provider_with_base("https://api.anthropic.com/");
        assert_eq!(
            provider.messages_url,
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn messages_url_preserves_full_path() {
        let provider = test_provider_with_base("https://api.anthropic.com/v1/messages");
        assert_eq!(
            provider.messages_url,
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn messages_url_appends_only_messages_to_v1_base() {
        // base_url 只给到版本根 /v1：补 /messages 而非重复拼 /v1
        let provider = test_provider_with_base("https://api.anthropic.com/v1");
        assert_eq!(
            provider.messages_url,
            "https://api.anthropic.com/v1/messages"
        );
    }

    /// 构建未发送的请求并检查其头与目标 URL（覆盖 post_builder 的完整装配）
    #[test]
    fn request_headers_carry_auth_and_version() {
        let provider = test_provider_with_base("https://api.test.com");
        let request = provider
            .post_builder(&serde_json::json!({"model": "m"}))
            .build()
            .expect("请求构建失败");

        assert_eq!(request.url().as_str(), "https://api.test.com/v1/messages");
        let headers = request.headers();
        assert_eq!(headers.get("x-api-key").unwrap(), "test-key");
        assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
    }

    /// 静默连接（响应头已到、body 永不到达）触发逐块空闲超时，
    /// 映射为可重试的 Timeout 错误后流终止
    #[tokio::test(start_paused = true)]
    async fn stream_chat_idle_timeout_yields_retryable_timeout() {
        // 自造静默服务器：回 200 响应头后不再发送任何字节
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            // 读完请求（头 + body），避免客户端写侧阻塞
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                      Transfer-Encoding: chunked\r\n\r\n",
                )
                .await
                .unwrap();
            // 持有连接但不发送任何数据：模拟代理掐断后的静默连接
            futures_util::future::pending::<()>().await;
        });

        let provider = AnthropicProvider::from_parts(
            "test-key".to_string(),
            format!("http://{addr}"),
            Client::new(),
        );
        let mut stream = provider.stream_chat(
            ChatRequest::default(),
            "claude-test",
            StreamOptions::default(),
        );

        let first = stream.next().await.expect("空闲超时应产出错误事件");
        assert!(
            matches!(first, Err(StreamError::Timeout)),
            "静默连接应映射为可重试超时：{first:?}"
        );
        assert!(
            stream.next().await.is_none(),
            "超时后流应终止，不再产出事件"
        );
    }

    fn test_provider_with_base(base_url: &str) -> AnthropicProvider {
        let client = Client::new();
        AnthropicProvider::from_parts("test-key".to_string(), base_url.to_string(), client)
    }
}
