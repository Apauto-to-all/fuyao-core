//! Anthropic Messages 协议 Provider 实现（薄 HTTP adapter）
//!
//! 基于 reqwest 直接请求 `/v1/messages`。本模块只承担 HTTP 交互——
//! 请求体编码（[`request`]）、流式线解码（[`sse`]）、HTTP 错误分类（[`classify`]）、
//! 非流式响应解析（[`completion`]）均已拆为独立纯函数模块，各自可单测；
//! 构造解析、POST 装配、发送链与带空闲超时的流读取共用 crate 根层共享骨架
//!（[`crate::http`]），本侧只注入协议差异项（默认端点、鉴权头组、错误分类）。
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
use fuyao_api::AgentPaths;
use reqwest::Client;

/// 协议要求的固定版本头取值
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic Messages 协议 Provider
///
/// 通过 reqwest 直接发送 HTTP 请求到 Anthropic 兼容 API。
/// 本结构只持有 HTTP 交互所需的客户端与鉴权信息；请求构造、流解码、错误分类
/// 委托给同目录下的纯函数模块，发送链与流读取走 crate 共享骨架。
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
    /// 共享构造骨架解析 API Key、base_url（缺省回退 Anthropic 官方端点）与
    /// reqwest Client（超时取全局配置 `llm`），失败路径 WARN 后返回 None。
    pub fn new(provider_id: &str, agent_paths: &AgentPaths) -> Option<Self> {
        let parts =
            crate::http::resolve_http_parts(provider_id, agent_paths, "https://api.anthropic.com")?;
        Some(Self::from_parts(
            parts.api_key,
            parts.base_url,
            parts.client,
        ))
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
    /// 非流式 [`Provider::chat`] 与流式 [`Provider::stream_chat`] 两条发送路径共用。
    /// 装配逻辑在共享骨架 [`crate::http::post_json`]，本侧只注入鉴权头组
    ///（`x-api-key` + 固定版本头）。返回 owned `RequestBuilder`——可在
    /// `async_stream` 块**外**构造、块内再 send，无需跨越 yield 持有 `&self`。
    fn post_builder(&self, body: &serde_json::Value) -> reqwest::RequestBuilder {
        crate::http::post_json(
            &self.client,
            &self.messages_url,
            &[
                ("x-api-key", self.api_key.as_str()),
                ("anthropic-version", ANTHROPIC_VERSION),
            ],
            body,
        )
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
            // 发送 + 网络错误映射 + 状态码校验 + 错误分类（与 chat() 共用共享发送链）
            let response = match crate::http::execute(request_builder, classify::classify_http_error).await {
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
                // 带空闲超时取下一块：静默连接不再永久挂起
                let item = match crate::http::next_chunk(&mut bytes_stream).await {
                    Ok(item) => item,
                    Err(e) => {
                        yield Err(e);
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
        let response =
            crate::http::execute(self.post_builder(&body), classify::classify_http_error).await?;

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
    use futures_util::StreamExt;

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
