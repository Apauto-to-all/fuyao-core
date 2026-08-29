//! OpenAI 兼容 Provider 实现（薄 HTTP adapter）
//!
//! 基于 reqwest 直接请求 `/chat/completions`。本模块只承担 HTTP 交互——
//! 请求体编码（[`request`]）、SSE 线解码（[`sse`]）、HTTP 错误分类（[`classify`]）、
//! 非流式响应模型（[`completion`]）均已拆为独立纯函数模块，各自可单测；
//! 构造解析、POST 装配、发送链与带空闲超时的流读取共用 crate 根层共享骨架
//!（[`crate::http`]），本侧只注入协议差异项（默认端点、Bearer 鉴权头、错误分类）。
//!
//! 支持所有 OpenAI 兼容供应商（DeepSeek、Qwen、Moonshot 等）：
//! - 流式/非流式对话
//! - 思考模式（reasoning_content / reasoning 字段）
//! - 工具调用（增量拼接）
//! - usage 统计（含 reasoning_tokens / cached_tokens）
//! - 错误分类（RateLimit / AuthError / ContextOverflow 等）

mod classify;
mod completion;
mod request;
mod sse;

use crate::provider::{
    BoxStream, ChatRequest, ChatResponse, FinishReason as ProviderFinishReason, Provider,
    StreamError, StreamEvent, StreamOptions as ProviderStreamOptions, StreamUsage,
};
use crate::sse::LineAssembler;
use async_trait::async_trait;
use fuyao_api::{AgentPaths, ToolCallData};
use reqwest::Client;

/// OpenAI 兼容 Provider
///
/// 通过 reqwest 直接发送 HTTP 请求到 OpenAI 兼容 API。
/// 本结构只持有 HTTP 交互所需的客户端与鉴权信息；请求构造、流解码、错误分类
/// 委托给同目录下的纯函数模块，发送链与流读取走 crate 共享骨架。
pub struct OpenAIProvider {
    /// HTTP 客户端（复用连接池）
    client: Client,
    /// chat completions 完整请求 URL（构造时定形，Provider 生命周期内不变）
    chat_url: String,
    /// 预拼装的 Bearer 鉴权头值（构造时定形，避免每请求重复分配）
    auth_header: String,
}

impl OpenAIProvider {
    /// 构造不变的请求要素：完整 URL 与鉴权头值
    ///
    /// 两者只依赖 api_key / base_url，在 Provider 生命周期内不变，
    /// 构造时一次算好，每次请求直接复用。
    fn build_request_parts(api_key: &str, base_url: &str) -> (String, String) {
        let base = base_url.trim_end_matches('/');
        // 如果 base_url 已包含 /chat/completions 则直接使用
        let chat_url = if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        };
        (chat_url, format!("Bearer {api_key}"))
    }

    /// 从 provider_id 和 agent_paths 创建
    ///
    /// 共享构造骨架解析 API Key、base_url（缺省回退 OpenAI 官方端点）与
    /// reqwest Client（超时取全局配置 `llm`），失败路径 WARN 后返回 None。
    pub fn new(provider_id: &str, agent_paths: &AgentPaths) -> Option<Self> {
        let parts =
            crate::http::resolve_http_parts(provider_id, agent_paths, "https://api.openai.com/v1")?;
        let (chat_url, auth_header) = Self::build_request_parts(&parts.api_key, &parts.base_url);
        Some(Self {
            client: parts.client,
            chat_url,
            auth_header,
        })
    }

    /// 从已有配置创建（用于测试）
    pub fn from_parts(api_key: String, base_url: String, client: Client) -> Self {
        let (chat_url, auth_header) = Self::build_request_parts(&api_key, &base_url);
        Self {
            client,
            chat_url,
            auth_header,
        }
    }

    /// 构造已带 url + auth + content-type 的 POST 请求构建器（未发送）
    ///
    /// 非流式 [`Provider::chat`] 与流式 [`Provider::stream_chat`] 两条发送路径共用。
    /// 装配逻辑在共享骨架 [`crate::http::post_json`]，本侧只注入 Bearer 鉴权头。
    /// 返回 owned `RequestBuilder`——可在 `async_stream` 块**外**构造、块内再 send，
    /// 无需跨越 yield 持有 `&self`（这正是 stream_chat 不能直接调 `&self` 异步方法的原因）。
    fn post_builder(&self, body: &serde_json::Value) -> reqwest::RequestBuilder {
        crate::http::post_json(
            &self.client,
            &self.chat_url,
            &[("Authorization", self.auth_header.as_str())],
            body,
        )
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn stream_chat(
        &self,
        request: ChatRequest,
        model: &str,
        options: ProviderStreamOptions,
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

            // 逐 chunk 消费 SSE 字节流，每次取 chunk 限定空闲超时窗口
            let mut bytes_stream = response.bytes_stream();
            // SSE 行组装器：字节上按 \n 切行，跨 chunk 的半行 / 半个多字节
            // 字符滞留其内部缓冲等续包，行内非法 UTF-8 以替换字符顶替
            let mut assembler = LineAssembler::new();

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

                // 切出本 chunk 内所有完整行并逐行线解码；坏行（JSON 解析失败）
                // 跳过不中断，后续行照常解码
                for line in assembler.push(&bytes) {
                    if let Some(chunk) = sse::parse_sse_line(&line) {
                        for event in sse::extract_stream_events(&chunk) {
                            yield Ok(event);
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
        options: ProviderStreamOptions,
    ) -> Result<ChatResponse, StreamError> {
        let started = std::time::Instant::now();
        let body = request::build_request_body(request, model, &options, false);
        let response =
            crate::http::execute(self.post_builder(&body), classify::classify_http_error).await?;

        let response_text = response
            .text()
            .await
            .map_err(|e| StreamError::StreamParseError(format!("读取响应体失败: {e}")))?;

        let api_response: completion::ChatCompletionResponse = serde_json::from_str(&response_text)
            .map_err(|e| StreamError::StreamParseError(format!("非流式响应 JSON 解析失败: {e}")))?;

        // 提取第一个 choice
        let choice =
            api_response
                .choices
                .into_iter()
                .next()
                .ok_or_else(|| StreamError::ApiError {
                    status: None,
                    message: "响应中无 choice".to_string(),
                })?;

        // 解析 finish_reason
        let finish_reason = match choice.finish_reason.as_deref() {
            Some("stop") => ProviderFinishReason::Stop,
            Some("tool_calls") => ProviderFinishReason::ToolCalls,
            Some("length") => ProviderFinishReason::Length,
            _ => ProviderFinishReason::Stop,
        };

        // 解析工具调用
        let tool_calls = choice.message.tool_calls.map(|calls| {
            calls
                .into_iter()
                .filter_map(|tc| {
                    let func = tc.function?;
                    Some(ToolCallData {
                        id: tc.id.unwrap_or_default(),
                        name: func.name.unwrap_or_default(),
                        arguments: func.arguments.unwrap_or_default(),
                    })
                })
                .collect::<Vec<_>>()
        });

        // 解析 usage
        let usage = match api_response.usage {
            Some(u) => StreamUsage {
                prompt_tokens: u.prompt_tokens.unwrap_or(0),
                completion_tokens: u.completion_tokens.unwrap_or(0),
                total_tokens: u.total_tokens.unwrap_or(0),
                completion_reasoning_tokens: u
                    .completion_tokens_details
                    .and_then(|d| d.reasoning_tokens),
                prompt_cached_tokens: u.prompt_tokens_details.and_then(|d| d.cached_tokens),
                prompt_cache_creation_tokens: None,
            },
            None => StreamUsage::default(),
        };

        tracing::info!(
            model = %model,
            elapsed_ms = started.elapsed().as_millis() as u64,
            tokens_in = usage.prompt_tokens,
            tokens_out = usage.completion_tokens,
            thinking = usage.completion_reasoning_tokens.unwrap_or(0) > 0,
            "LLM 请求完成"
        );

        Ok(ChatResponse {
            content: choice.message.content,
            reasoning: choice.message.reasoning_content,
            tool_calls,
            usage,
            finish_reason,
        })
    }
}

// URL / 鉴权头预计算等保留在 adapter 上的纯字符串操作；其余职责的测试见各子模块
#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[test]
    fn chat_url_appends_path() {
        let provider = test_provider_with_base("https://api.test.com/v1");
        assert_eq!(
            provider.chat_url,
            "https://api.test.com/v1/chat/completions"
        );
    }

    #[test]
    fn chat_url_preserves_full_path() {
        let provider = test_provider_with_base("https://api.test.com/v1/chat/completions");
        assert_eq!(
            provider.chat_url,
            "https://api.test.com/v1/chat/completions"
        );
    }

    #[test]
    fn chat_url_strips_trailing_slash() {
        let provider = test_provider_with_base("https://api.test.com/v1/");
        assert_eq!(
            provider.chat_url,
            "https://api.test.com/v1/chat/completions"
        );
    }

    #[test]
    fn auth_header_prebuilt() {
        let provider = test_provider_with_base("https://api.test.com/v1");
        assert_eq!(provider.auth_header, "Bearer test-key");
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

        let provider = OpenAIProvider::from_parts(
            "test-key".to_string(),
            format!("http://{addr}/v1"),
            Client::new(),
        );
        let mut stream = provider.stream_chat(
            ChatRequest::default(),
            "test-model",
            ProviderStreamOptions::default(),
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

    fn test_provider_with_base(base_url: &str) -> OpenAIProvider {
        let client = Client::new();
        OpenAIProvider::from_parts("test-key".to_string(), base_url.to_string(), client)
    }
}
