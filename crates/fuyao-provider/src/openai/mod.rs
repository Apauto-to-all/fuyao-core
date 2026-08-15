//! OpenAI 兼容 Provider 实现（薄 HTTP adapter）
//!
//! 基于 reqwest 直接请求 `/chat/completions`。本模块只承担 HTTP 交互——
//! 请求体编码（[`request`]）、SSE 线解码（[`sse`]）、HTTP 错误分类（[`classify`]）、
//! 非流式响应模型（[`completion`]）均已拆为独立纯函数模块，各自可单测。
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
use async_trait::async_trait;
use futures_util::StreamExt;
use fuyao_api::{AgentPaths, ToolCallData};
use reqwest::Client;
use std::time::Duration;

/// OpenAI 兼容 Provider
///
/// 通过 reqwest 直接发送 HTTP 请求到 OpenAI 兼容 API。
/// 本结构只持有 HTTP 交互所需的客户端与鉴权信息；请求构造、流解码、错误分类
/// 委托给同目录下的纯函数模块。
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
    /// 从注册表解析 api_key 和 base_url，构建 reqwest Client。
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
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

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

        let (chat_url, auth_header) = Self::build_request_parts(&api_key, &base_url);
        Some(Self {
            client,
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
    /// 非流式 [`Provider::chat`] 与流式 [`Provider::stream_chat`] 两条发送路径共用，
    /// 避免 auth header / content-type / url 装配逻辑两处复制。返回 owned
    /// `RequestBuilder`——可在 `async_stream` 块**外**构造、块内再 send，无需跨越
    /// yield 持有 `&self`（这正是 stream_chat 不能直接调 `&self` 异步方法的原因）。
    fn post_builder(&self, body: &serde_json::Value) -> reqwest::RequestBuilder {
        self.client
            .post(&self.chat_url)
            .header("Authorization", self.auth_header.as_str())
            .header("Content-Type", "application/json")
            .json(body)
    }

    /// 发送请求 + 错误分类（url/auth 装配之后的完整发送链路）
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
            let mut assembler = sse::LineAssembler::new();

            while let Some(item) = bytes_stream.next().await {
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
                    match sse::parse_sse_line(&line) {
                        Ok(Some(chunk)) => {
                            for event in sse::extract_stream_events(&chunk) {
                                yield Ok(event);
                            }
                        }
                        Ok(None) => {} // 空行或 [DONE]
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
        options: ProviderStreamOptions,
    ) -> Result<ChatResponse, StreamError> {
        let started = std::time::Instant::now();
        let body = request::build_request_body(request, model, &options, false);
        let response = Self::execute(self.post_builder(&body)).await?;

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

    fn test_provider_with_base(base_url: &str) -> OpenAIProvider {
        let client = Client::new();
        OpenAIProvider::from_parts("test-key".to_string(), base_url.to_string(), client)
    }
}
