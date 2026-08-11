//! OpenAI 兼容 Provider 实现
//!
//! 基于 reqwest 直接请求 `/chat/completions`，自定义 serde 类型解析 SSE 流。
//! 不依赖 async-openai，完整支持：
//! - 流式/非流式对话
//! - 思考模式（reasoning_content / reasoning 字段）
//! - 工具调用（增量拼接）
//! - usage 统计（含 reasoning_tokens / cached_tokens）
//! - 错误分类（RateLimit / AuthError / ContextOverflow 等）

use crate::StreamError;
use crate::provider::{
    BoxStream, ChatRequest, ChatResponse, FinishReason as ProviderFinishReason, Provider,
    StreamEvent, StreamOptions as ProviderStreamOptions, StreamUsage, ToolCallData,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use fuyao_api::{AgentPaths, ImageContent, MessageRole};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

// =========== OpenAI Provider ===========

/// OpenAI 协议支持的图片 MIME 白名单（仅图像：PNG / JPEG / WEBP / 非动画 GIF）
const SUPPORTED_IMAGE_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// 单图解码后字节上限（OpenAI 协议约束；base64 长度 /4*3 估算解码字节数）
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// 校验并过滤图片：MIME 白名单 + 协议字节上限，不合规的丢弃并告警
///
/// 校验失败只丢单张图、不中断整条消息——图像是辅助信息，文本对话照常。
fn filter_valid_images(images: &[ImageContent]) -> Vec<&ImageContent> {
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

/// OpenAI 兼容 Provider
///
/// 通过 reqwest 直接发送 HTTP 请求到 OpenAI 兼容 API。
/// 支持所有 OpenAI 兼容供应商（DeepSeek、Qwen、Moonshot 等）。
pub struct OpenAIProvider {
    /// HTTP 客户端（复用连接池）
    client: Client,
    /// API 密钥
    api_key: String,
    /// API 基础 URL（如 <https://api.openai.com/v1>）
    base_url: String,
}

impl OpenAIProvider {
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

        Some(Self {
            client,
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    /// 从已有配置创建（用于测试）
    pub fn from_parts(api_key: String, base_url: String, client: Client) -> Self {
        Self {
            client,
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// 构建 chat completions 请求 URL
    fn chat_url(&self) -> String {
        // 如果 base_url 已包含 /chat/completions 则直接使用
        if self.base_url.ends_with("/chat/completions") {
            return self.base_url.clone();
        }
        format!("{}/chat/completions", self.base_url)
    }

    /// 将内部 ChatRequest 转换为 OpenAI API 请求体
    fn build_request_body(
        &self,
        request: ChatRequest,
        model: &str,
        options: &ProviderStreamOptions,
        stream: bool,
    ) -> serde_json::Value {
        let mut messages = Vec::new();

        // 系统消息
        if let Some(system) = request.system {
            messages.push(serde_json::json!({
                "role": "system",
                "content": system,
            }));
        }

        // 对话消息
        for msg in request.messages {
            let mut msg_value = serde_json::json!({
                "role": msg.role.as_str(),
            });

            // 内容：带图 user 消息转 parts 数组（text + image_url），纯文本保持字符串（零回归）
            let valid_images = if matches!(msg.role, MessageRole::User) {
                filter_valid_images(&msg.images)
            } else {
                if !msg.images.is_empty() {
                    tracing::warn!(role = %msg.role.as_str(), "非 user 消息携带图片，已忽略");
                }
                vec![]
            };

            if valid_images.is_empty() {
                // 无图（含图片全部被过滤）：保持原纯字符串形态
                if let Some(content) = &msg.content {
                    msg_value["content"] = serde_json::Value::String(content.clone());
                }
            } else {
                // 有图：content 升级为 parts 数组，图片以 data URL 内联
                let mut parts: Vec<serde_json::Value> = Vec::new();
                if let Some(text) = &msg.content
                    && !text.is_empty()
                {
                    parts.push(serde_json::json!({ "type": "text", "text": text }));
                }
                for img in valid_images {
                    parts.push(serde_json::json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{}", img.mime_type, img.data),
                        }
                    }));
                }
                msg_value["content"] = serde_json::Value::Array(parts);
            }

            // 思考内容（部分供应商需要在历史消息中传递）
            if let Some(reasoning) = &msg.reasoning {
                msg_value["reasoning_content"] = serde_json::Value::String(reasoning.clone());
            }

            // 工具调用
            if let Some(tool_calls) = &msg.tool_calls {
                msg_value["tool_calls"] = serde_json::Value::Array(
                    tool_calls.iter().map(|tc| {
                        let mut tc_val = serde_json::json!({
                            "id": tc.get("id"),
                            "type": "function",
                            "function": {
                                "name": tc.get("function").and_then(|f| f.get("name")),
                                "arguments": tc.get("function").and_then(|f| f.get("arguments")),
                            }
                        });
                        // 保留 extra_content 等扩展字段
                        if let Some(id) = tc.get("id") {
                            tc_val["id"] = id.clone();
                        }
                        tc_val
                    }).collect(),
                );
            }

            // 工具调用 ID
            if let Some(tool_call_id) = &msg.tool_call_id {
                msg_value["tool_call_id"] = serde_json::Value::String(tool_call_id.clone());
            }

            messages.push(msg_value);
        }

        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
        });

        // 流式
        if stream {
            body["stream"] = serde_json::Value::Bool(true);
            body["stream_options"] = serde_json::json!({
                "include_usage": true,
            });
        }

        // 温度
        if let Some(temp) = options.temperature {
            body["temperature"] = serde_json::Number::from_f64(temp)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null);
        }

        // 工具
        if let Some(tools) = &options.tools
            && !tools.is_empty()
        {
            body["tools"] = serde_json::Value::Array(tools.clone());
            body["tool_choice"] = serde_json::Value::String("auto".to_string());
        }

        // 工具选择
        if let Some(tool_choice) = &options.tool_choice {
            body["tool_choice"] = tool_choice.clone();
        }

        // 思考字段独立注入：thinking_type 与 reasoning_effort 是两个正交字段，
        // 各自为 Some 时各自发送，互不压制。配置了就必须发——禁止因 thinking_type=Disabled
        // 而压掉 reasoning_effort，两者由服务器各自解释，fuyao 不替服务器做语义裁剪。
        if let Some(t) = &options.thinking_type {
            body["thinking"] = serde_json::json!({
                "type": serde_json::to_value(t).expect("ThinkingType 序列化不会失败")
            });
        }
        if let Some(e) = &options.reasoning_effort {
            body["reasoning_effort"] = serde_json::Value::String(e.clone());
        }

        body
    }

    /// 构造已带 url + auth + content-type 的 POST 请求构建器（未发送）
    ///
    /// 非流式 [`Provider::chat`] 与流式 [`Provider::stream_chat`] 两条发送路径共用，
    /// 避免 auth header / content-type / url 装配逻辑两处复制。返回 owned
    /// `RequestBuilder`——可在 `async_stream` 块**外**构造、块内再 send，无需跨越
    /// yield 持有 `&self`（这正是 stream_chat 不能直接调 `&self` 异步方法的原因）。
    fn post_builder(&self, body: &serde_json::Value) -> reqwest::RequestBuilder {
        self.client
            .post(self.chat_url())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(body)
    }

    /// 发送请求 + 错误分类（url/auth 装配之后的完整发送链路）
    ///
    /// 接收 [`Self::post_builder`] 产出的构建器，执行 send → 网络错误映射（timeout /
    /// connection）→ HTTP 状态码校验 → [`Self::classify_http_error`]。两条发送路径
    /// 共用此方法，消除原先 stream_chat 内联的「send + timeout 映射 + status 校验 +
    /// classify」与 send_request 的复制。
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
            return Err(Self::classify_http_error(status_code, &body_text));
        }

        Ok(response)
    }

    /// 根据 HTTP 状态码和响应体分类错误
    fn classify_http_error(status_code: u16, body: &str) -> StreamError {
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
                if body.contains("context_length_exceeded")
                    || body.contains("maximum context length")
                {
                    return StreamError::ContextOverflow;
                }
                StreamError::ApiError {
                    status: Some(status_code),
                    message: format!("HTTP {status_code}: {body}"),
                }
            }
        }
    }
}

// =========== SSE 流解析 ===========

/// 解析 SSE 行，返回反序列化后的 chunk
///
/// SSE 格式：`data: {json}\n` 或 `data: [DONE]\n`
fn parse_sse_line(line: &str) -> Result<Option<serde_json::Value>, StreamError> {
    let line = line.trim();

    // 空行或注释行
    if line.is_empty() || line.starts_with(':') {
        return Ok(None);
    }

    // 提取 data 前缀后的内容
    let data = match line.strip_prefix("data:") {
        Some(d) => d.trim(),
        None => return Ok(None),
    };

    // 流结束标记
    if data == "[DONE]" {
        return Ok(None);
    }

    // 反序列化 JSON
    serde_json::from_str(data)
        .map(Some)
        .map_err(|e| StreamError::StreamParseError(format!("SSE JSON 解析失败: {e}")))
}

/// 从 SSE chunk 中提取流式事件
fn extract_stream_events(chunk: &serde_json::Value) -> Vec<StreamEvent> {
    let mut events = Vec::new();

    let choices = match chunk.get("choices").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => {
            // choices 为空或不存在时，检查顶层 usage（include_usage 的最后一个 chunk）
            let usage = extract_usage(chunk);
            if usage.total_tokens > 0 || usage.prompt_tokens > 0 || usage.completion_tokens > 0 {
                events.push(StreamEvent::Done {
                    usage,
                    finish_reason: ProviderFinishReason::Stop,
                });
            }
            return events;
        }
    };

    // choices 为空数组时（include_usage 的最终 chunk：choices=[], usage={...}）
    if choices.is_empty() {
        let usage = extract_usage(chunk);
        if usage.total_tokens > 0 || usage.prompt_tokens > 0 || usage.completion_tokens > 0 {
            events.push(StreamEvent::Done {
                usage,
                finish_reason: ProviderFinishReason::Stop,
            });
        }
        return events;
    }

    for choice in choices {
        let delta = match choice.get("delta") {
            Some(d) => d,
            None => continue,
        };

        // 文本内容增量
        if let Some(content) = delta.get("content").and_then(|c| c.as_str())
            && !content.is_empty()
        {
            events.push(StreamEvent::TextDelta {
                content: content.to_string(),
            });
        }

        // 思考内容增量
        // 兼容两种字段名：reasoning_content（DeepSeek/Qwen）和 reasoning（部分供应商）
        let reasoning = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(|r| r.as_str())
            .unwrap_or("");
        if !reasoning.is_empty() {
            events.push(StreamEvent::ReasoningDelta {
                content: reasoning.to_string(),
            });
        }

        // 工具调用增量（按 index 增量拼接，id/name/arguments 独立到达）
        if let Some(tool_calls) = delta.get("tool_calls").and_then(|tc| tc.as_array()) {
            for tc in tool_calls {
                let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;

                let id = tc
                    .get("id")
                    .and_then(|i| i.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());

                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());

                let args_delta = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());

                if id.is_some() || name.is_some() || args_delta.is_some() {
                    events.push(StreamEvent::ToolCallChunk {
                        index,
                        id,
                        name,
                        args_delta,
                    });
                }
            }
        }

        // 流结束
        if let Some(finish_reason) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            let reason = match finish_reason {
                "stop" => ProviderFinishReason::Stop,
                "tool_calls" => ProviderFinishReason::ToolCalls,
                "length" => ProviderFinishReason::Length,
                _ => ProviderFinishReason::Stop,
            };

            // 从 chunk 顶层提取 usage
            let usage = extract_usage(chunk);
            events.push(StreamEvent::Done {
                usage,
                finish_reason: reason,
            });
        }
    }

    events
}

/// 从 chunk 中提取 usage 统计
fn extract_usage(chunk: &serde_json::Value) -> StreamUsage {
    let usage = match chunk.get("usage") {
        Some(u) => u,
        None => return StreamUsage::default(),
    };

    StreamUsage {
        prompt_tokens: usage
            .get("prompt_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        completion_tokens: usage
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        total_tokens: usage
            .get("total_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        completion_reasoning_tokens: usage
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        prompt_cached_tokens: usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
    }
}

// =========== 非流式响应类型 ===========

/// 非流式 API 响应
#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatCompletionChoice>,
    #[serde(default)]
    usage: Option<ChatCompletionUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChoice {
    message: ChatCompletionMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// 非流式消息（含思考内容）
#[derive(Debug, Deserialize)]
struct ChatCompletionMessage {
    #[serde(default)]
    content: Option<String>,
    /// 思考模式：reasoning_content 字段（DeepSeek / Qwen 等）
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChatCompletionToolCall>>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionToolCall {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ChatCompletionFunction>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionUsage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
    #[serde(default)]
    total_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
}

// =========== 辅助函数 ===========

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

// =========== Provider trait 实现 ===========

#[async_trait]
impl Provider for OpenAIProvider {
    fn stream_chat(
        &self,
        request: ChatRequest,
        model: &str,
        options: ProviderStreamOptions,
    ) -> BoxStream<Result<StreamEvent, StreamError>> {
        let body = self.build_request_body(request, model, &options, true);
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
            // SSE 行缓冲区
            let mut line_buf = String::new();
            // UTF-8 不完整序列缓冲区（处理跨 chunk 的多字节字符）
            let mut utf8_buf: Vec<u8> = Vec::new();

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

                // 处理 UTF-8 编码
                utf8_buf.extend_from_slice(&bytes);
                let text = match std::str::from_utf8(&utf8_buf) {
                    Ok(s) => {
                        let owned = s.to_string();
                        utf8_buf.clear();
                        owned
                    }
                    Err(e) => {
                        let valid_up_to = e.valid_up_to();
                        if valid_up_to == 0 && utf8_buf.len() < 4 {
                            // 可能是不完整的多字节字符，等待更多数据
                            continue;
                        }
                        let valid =
                            String::from_utf8_lossy(&utf8_buf[..valid_up_to]).into_owned();
                        utf8_buf.drain(..valid_up_to);
                        valid
                    }
                };

                if text.is_empty() {
                    continue;
                }

                line_buf.push_str(&text);

                // 按换行符切割 SSE 行
                while let Some(pos) = line_buf.find('\n') {
                    let line = line_buf[..pos].to_string();
                    line_buf.drain(..=pos);

                    match parse_sse_line(&line) {
                        Ok(Some(chunk)) => {
                            for event in extract_stream_events(&chunk) {
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
        let body = self.build_request_body(request, model, &options, false);
        let response = Self::execute(self.post_builder(&body)).await?;

        let response_text = response
            .text()
            .await
            .map_err(|e| StreamError::StreamParseError(format!("读取响应体失败: {e}")))?;

        let api_response: ChatCompletionResponse = serde_json::from_str(&response_text)
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

// =========== 单元测试 ===========

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatMessage, ChatRequest, StreamOptions as ProviderStreamOptions};
    use fuyao_api::{MessageRole, ThinkingType};

    #[test]
    fn chat_url_appends_path() {
        let provider = test_provider_with_base("https://api.test.com/v1");
        assert_eq!(
            provider.chat_url(),
            "https://api.test.com/v1/chat/completions"
        );
    }

    #[test]
    fn chat_url_preserves_full_path() {
        let provider = test_provider_with_base("https://api.test.com/v1/chat/completions");
        assert_eq!(
            provider.chat_url(),
            "https://api.test.com/v1/chat/completions"
        );
    }

    #[test]
    fn chat_url_strips_trailing_slash() {
        let provider = test_provider_with_base("https://api.test.com/v1/");
        assert_eq!(
            provider.chat_url(),
            "https://api.test.com/v1/chat/completions"
        );
    }

    #[test]
    fn build_request_body_image_message_uses_parts_array() {
        // 带图 user 消息：content 升级为 parts 数组（text + image_url data URL）
        let provider = test_provider();
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("看图".to_string()),
                images: vec![ImageContent {
                    mime_type: "image/png".into(),
                    data: "aGVsbG8=".into(),
                }],
                ..Default::default()
            }],
            system: None,
        };
        let body = provider.build_request_body(
            request,
            "qwen3.6-plus",
            &ProviderStreamOptions::default(),
            false,
        );

        let content = &body["messages"][0]["content"];
        assert!(content.is_array(), "带图消息 content 应为数组");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "看图");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            "data:image/png;base64,aGVsbG8="
        );
    }

    #[test]
    fn build_request_body_image_message_without_text_omits_text_part() {
        // content 为空时只发图片 part
        let provider = test_provider();
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: None,
                images: vec![ImageContent {
                    mime_type: "image/webp".into(),
                    data: "d2VicA==".into(),
                }],
                ..Default::default()
            }],
            system: None,
        };
        let body = provider.build_request_body(
            request,
            "qwen3.6-plus",
            &ProviderStreamOptions::default(),
            false,
        );

        let content = &body["messages"][0]["content"];
        assert_eq!(content.as_array().map(Vec::len), Some(1));
        assert_eq!(content[0]["type"], "image_url");
    }

    #[test]
    fn build_request_body_drops_unsupported_mime_image() {
        // 非法 MIME 图片被丢弃：消息回退为纯文本字符串形态
        let provider = test_provider();
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("文本".to_string()),
                images: vec![
                    ImageContent {
                        mime_type: "application/pdf".into(),
                        data: "x".into(),
                    },
                    ImageContent {
                        mime_type: "image/png".into(),
                        data: "aGVsbG8=".into(),
                    },
                ],
                ..Default::default()
            }],
            system: None,
        };
        let body = provider.build_request_body(
            request,
            "qwen3.6-plus",
            &ProviderStreamOptions::default(),
            false,
        );

        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content.as_array().map(Vec::len), Some(2), "pdf 图应被丢弃");
    }

    #[test]
    fn build_request_body_drops_oversized_image() {
        // 超过 20MB 协议上限的图片被丢弃（base64 长度 = 解码字节 × 4/3）
        let provider = test_provider();
        let oversized = "A".repeat(MAX_IMAGE_BYTES / 3 * 4 + 100);
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("大图".to_string()),
                images: vec![ImageContent {
                    mime_type: "image/png".into(),
                    data: oversized,
                }],
                ..Default::default()
            }],
            system: None,
        };
        let body = provider.build_request_body(
            request,
            "qwen3.6-plus",
            &ProviderStreamOptions::default(),
            false,
        );

        let content = &body["messages"][0]["content"];
        assert_eq!(content, "大图", "超限图被丢弃后应回退为纯字符串");
    }

    #[test]
    fn build_request_body_ignores_images_on_non_user_messages() {
        // 防御：非 user 角色携带图片时忽略（协议侧 tool/assistant content 只认字符串）
        let provider = test_provider();
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::Assistant,
                content: Some("回复".to_string()),
                images: vec![ImageContent {
                    mime_type: "image/png".into(),
                    data: "aGVsbG8=".into(),
                }],
                ..Default::default()
            }],
            system: None,
        };
        let body = provider.build_request_body(
            request,
            "qwen3.6-plus",
            &ProviderStreamOptions::default(),
            false,
        );

        assert_eq!(body["messages"][0]["content"], "回复");
    }

    #[test]
    fn build_request_body_includes_model_and_messages() {
        let provider = test_provider();
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("hello".to_string()),
                ..Default::default()
            }],
            system: None,
        };
        let body = provider.build_request_body(
            request,
            "qwen3.6-plus",
            &ProviderStreamOptions::default(),
            false,
        );

        assert_eq!(body["model"], "qwen3.6-plus");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
        assert!(body.get("stream").is_none());
    }

    #[test]
    fn build_request_body_stream_mode() {
        let provider = test_provider();
        let request = ChatRequest::default();
        let body = provider.build_request_body(
            request,
            "qwen3.6-plus",
            &ProviderStreamOptions::default(),
            true,
        );

        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn build_request_body_with_system() {
        let provider = test_provider();
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("hi".to_string()),
                ..Default::default()
            }],
            system: Some("你是助手".to_string()),
        };
        let body = provider.build_request_body(
            request,
            "qwen3.6-plus",
            &ProviderStreamOptions::default(),
            false,
        );

        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "你是助手");
        assert_eq!(body["messages"][1]["role"], "user");
    }

    #[test]
    fn build_request_body_with_tools() {
        let provider = test_provider();
        let request = ChatRequest::default();
        let options = ProviderStreamOptions {
            tools: Some(vec![serde_json::json!({
                "type": "function",
                "function": {
                    "name": "bash",
                    "description": "执行命令",
                    "parameters": {}
                }
            })]),
            ..Default::default()
        };
        let body = provider.build_request_body(request, "qwen3.6-plus", &options, false);

        assert!(body["tools"].is_array());
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn build_request_body_no_thinking_when_both_none() {
        // 思考模型默认（两参数都 None）：请求体不含思考字段
        let provider = test_provider();
        let request = ChatRequest::default();
        let options = ProviderStreamOptions {
            ..Default::default()
        };
        let body = provider.build_request_body(request, "deepseek-v4-flash", &options, false);
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn build_request_body_thinking_enabled_only() {
        // 开思考但不设强度：仅发 thinking:enabled
        let provider = test_provider();
        let request = ChatRequest::default();
        let options = ProviderStreamOptions {
            thinking_type: Some(ThinkingType::Enabled),
            ..Default::default()
        };
        let body = provider.build_request_body(request, "deepseek-v4-flash", &options, false);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn build_request_body_thinking_enabled_with_effort() {
        // 开思考 + 指定强度：两者都发
        let provider = test_provider();
        let request = ChatRequest::default();
        let options = ProviderStreamOptions {
            thinking_type: Some(ThinkingType::Enabled),
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        let body = provider.build_request_body(request, "deepseek-v4-flash", &options, false);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn build_request_body_accepts_arbitrary_effort_string() {
        // 自定义档位名（如 "big" 不在常见枚举内）原样透传，fuyao 不校验
        let provider = test_provider();
        let request = ChatRequest::default();
        let options = ProviderStreamOptions {
            thinking_type: Some(ThinkingType::Enabled),
            reasoning_effort: Some("big".to_string()),
            ..Default::default()
        };
        let body = provider.build_request_body(request, "weird-model", &options, false);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "big");
    }

    #[test]
    fn build_request_body_thinking_disabled_still_sends_effort() {
        // 两字段独立：thinking_type=Disabled 不压制 reasoning_effort，配了就发
        let provider = test_provider();
        let request = ChatRequest::default();
        let options = ProviderStreamOptions {
            thinking_type: Some(ThinkingType::Disabled),
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        let body = provider.build_request_body(request, "deepseek-v4-flash", &options, false);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn parse_sse_line_skips_empty_and_comments() {
        assert!(parse_sse_line("").unwrap().is_none());
        assert!(parse_sse_line(": comment").unwrap().is_none());
        assert!(parse_sse_line("data: [DONE]").unwrap().is_none());
    }

    #[test]
    fn parse_sse_line_parses_json() {
        let chunk = parse_sse_line(r#"data: {"choices":[]}"#).unwrap().unwrap();
        assert!(chunk.get("choices").unwrap().as_array().unwrap().is_empty());
    }

    #[test]
    fn parse_sse_line_rejects_invalid_json() {
        assert!(parse_sse_line("data: {invalid}").is_err());
    }

    #[test]
    fn extract_stream_events_text_delta() {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {"content": "Hello"},
                "finish_reason": null
            }]
        });
        let events = extract_stream_events(&chunk);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::TextDelta { content } if content == "Hello"));
    }

    #[test]
    fn extract_stream_events_reasoning_delta() {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {"reasoning_content": "思考中..."},
                "finish_reason": null
            }]
        });
        let events = extract_stream_events(&chunk);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], StreamEvent::ReasoningDelta { content } if content == "思考中...")
        );
    }

    #[test]
    fn extract_stream_events_reasoning_field_fallback() {
        // 部分供应商使用 "reasoning" 字段名
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {"reasoning": "推理内容"},
                "finish_reason": null
            }]
        });
        let events = extract_stream_events(&chunk);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], StreamEvent::ReasoningDelta { content } if content == "推理内容")
        );
    }

    #[test]
    fn extract_stream_events_tool_call_chunk_with_id_and_name() {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": {"name": "bash", "arguments": ""}
                    }]
                }
            }]
        });
        let events = extract_stream_events(&chunk);
        assert!(!events.is_empty());
        assert!(
            matches!(&events[0], StreamEvent::ToolCallChunk { id, name, args_delta, .. }
                if id.as_deref() == Some("call_1")
                && name.as_deref() == Some("bash")
                && args_delta.is_none())
        );
    }

    #[test]
    fn extract_stream_events_tool_call_chunk_args_only() {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {"arguments": "{\"path\":"}
                    }]
                }
            }]
        });
        let events = extract_stream_events(&chunk);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], StreamEvent::ToolCallChunk { args_delta, id, name, .. }
                if args_delta.as_deref() == Some("{\"path\":")
                && id.is_none()
                && name.is_none())
        );
    }

    #[test]
    fn extract_stream_events_tool_call_chunk_empty_fields_ignored() {
        // 所有字段为空时不应该产生事件
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "",
                        "function": {"name": "", "arguments": ""}
                    }]
                }
            }]
        });
        let events = extract_stream_events(&chunk);
        assert!(events.is_empty());
    }

    #[test]
    fn extract_stream_events_done() {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150
            }
        });
        let events = extract_stream_events(&chunk);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Done {
                usage,
                finish_reason,
            } => {
                assert_eq!(usage.prompt_tokens, 100);
                assert_eq!(usage.completion_tokens, 50);
                assert_eq!(*finish_reason, ProviderFinishReason::Stop);
            }
            _ => panic!("期望 Done 事件"),
        }
    }

    #[test]
    fn extract_stream_events_done_with_tool_calls() {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {},
                "finish_reason": "tool_calls"
            }]
        });
        let events = extract_stream_events(&chunk);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            StreamEvent::Done {
                finish_reason: ProviderFinishReason::ToolCalls,
                ..
            }
        ));
    }

    #[test]
    fn extract_usage_from_chunk() {
        let chunk = serde_json::json!({
            "usage": {
                "prompt_tokens": 200,
                "completion_tokens": 80,
                "total_tokens": 280,
                "completion_tokens_details": {"reasoning_tokens": 30},
                "prompt_tokens_details": {"cached_tokens": 50}
            }
        });
        let usage = extract_usage(&chunk);
        assert_eq!(usage.prompt_tokens, 200);
        assert_eq!(usage.completion_tokens, 80);
        assert_eq!(usage.total_tokens, 280);
        assert_eq!(usage.completion_reasoning_tokens, Some(30));
        assert_eq!(usage.prompt_cached_tokens, Some(50));
    }

    #[test]
    fn extract_usage_defaults_when_missing() {
        let chunk = serde_json::json!({});
        let usage = extract_usage(&chunk);
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 0);
    }

    #[test]
    fn extract_usage_defaults_when_null_fields() {
        let chunk = serde_json::json!({
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5
            }
        });
        let usage = extract_usage(&chunk);
        assert_eq!(usage.prompt_tokens, 10);
        assert!(usage.completion_reasoning_tokens.is_none());
        assert!(usage.prompt_cached_tokens.is_none());
    }

    #[test]
    fn classify_http_error_401() {
        let err = OpenAIProvider::classify_http_error(401, "unauthorized");
        assert!(matches!(err, StreamError::AuthError(_)));
    }

    #[test]
    fn classify_http_error_429() {
        let err = OpenAIProvider::classify_http_error(429, "rate limited");
        assert!(matches!(err, StreamError::RateLimit { .. }));
    }

    #[test]
    fn classify_http_error_context_overflow() {
        let err =
            OpenAIProvider::classify_http_error(400, "context_length_exceeded: too many tokens");
        assert!(matches!(err, StreamError::ContextOverflow));
    }

    #[test]
    fn classify_http_error_413_payload_too_large() {
        // 413 Payload Too Large：HTTP 状态码已明确表示请求体过大，
        // 不依赖 body 关键词匹配，直接归类为 ContextOverflow
        let err = OpenAIProvider::classify_http_error(413, "payload too large");
        assert!(matches!(err, StreamError::ContextOverflow));
    }

    #[test]
    fn classify_http_error_413_with_unexpected_body_still_overflow() {
        // 413 即便 body 是空字符串或非典型格式，仍是上下文溢出
        let err = OpenAIProvider::classify_http_error(413, "");
        assert!(matches!(err, StreamError::ContextOverflow));
    }

    #[test]
    fn classify_http_error_generic() {
        let err = OpenAIProvider::classify_http_error(500, "internal server error");
        assert!(matches!(
            err,
            StreamError::ApiError {
                status: Some(500),
                ..
            }
        ));
    }

    #[test]
    fn non_stream_response_deserializes() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": "Hello!",
                    "reasoning_content": "思考过程",
                    "tool_calls": null
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        }"#;
        let resp: ChatCompletionResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("Hello!"));
        assert_eq!(
            resp.choices[0].message.reasoning_content.as_deref(),
            Some("思考过程")
        );
        assert_eq!(resp.usage.as_ref().unwrap().prompt_tokens, Some(10));
    }

    #[test]
    fn non_stream_response_with_tool_calls() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"command\":\"ls\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let resp: ChatCompletionResponse = serde_json::from_str(json).unwrap();
        let msg = &resp.choices[0].message;
        assert!(msg.content.is_none());
        assert!(msg.tool_calls.is_some());
        let calls = msg.tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id.as_deref(), Some("call_1"));
        assert_eq!(
            calls[0].function.as_ref().unwrap().name.as_deref(),
            Some("bash")
        );
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

    #[test]
    fn extract_stream_events_empty_choices_with_usage() {
        // include_usage 的最终 chunk：choices 为空数组，usage 在顶层
        let chunk = serde_json::json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150,
                "completion_tokens_details": {"reasoning_tokens": 20},
                "prompt_tokens_details": {"cached_tokens": 30}
            }
        });
        let events = extract_stream_events(&chunk);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Done {
                usage,
                finish_reason,
            } => {
                assert_eq!(usage.prompt_tokens, 100);
                assert_eq!(usage.completion_tokens, 50);
                assert_eq!(usage.total_tokens, 150);
                assert_eq!(usage.completion_reasoning_tokens, Some(20));
                assert_eq!(usage.prompt_cached_tokens, Some(30));
                assert_eq!(*finish_reason, ProviderFinishReason::Stop);
            }
            _ => panic!("期望 Done 事件"),
        }
    }

    #[test]
    fn extract_stream_events_empty_choices_without_usage() {
        // choices 为空且无 usage → 不发任何事件
        let chunk = serde_json::json!({
            "choices": []
        });
        let events = extract_stream_events(&chunk);
        assert!(events.is_empty());
    }

    #[test]
    fn extract_stream_events_no_choices_with_usage() {
        // choices 字段不存在但有 usage
        let chunk = serde_json::json!({
            "usage": {
                "prompt_tokens": 50,
                "completion_tokens": 25,
                "total_tokens": 75
            }
        });
        let events = extract_stream_events(&chunk);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Done { usage, .. } => {
                assert_eq!(usage.prompt_tokens, 50);
                assert_eq!(usage.completion_tokens, 25);
            }
            _ => panic!("期望 Done 事件"),
        }
    }

    // 测试辅助函数
    fn test_provider() -> OpenAIProvider {
        let client = Client::new();
        OpenAIProvider::from_parts(
            "test-key".to_string(),
            "https://api.test.com/v1".to_string(),
            client,
        )
    }

    fn test_provider_with_base(base_url: &str) -> OpenAIProvider {
        let client = Client::new();
        OpenAIProvider::from_parts("test-key".to_string(), base_url.to_string(), client)
    }
}
