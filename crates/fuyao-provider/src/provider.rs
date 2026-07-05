//! LLM Provider 统一抽象
//!
//! 所有 LLM 供应商实现此 trait，返回统一的 StreamEvent 流。
//! Engine 层只消费 StreamEvent，不关心底层 chunk 格式。

use async_trait::async_trait;
use futures_util::Stream;
use fuyao_api::ThinkingType;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

/// 流式事件（Provider 层输出，Engine 层消费）
///
/// 每个 Provider 实现负责将自身 chunk 格式解码为此类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// 文本内容增量
    TextDelta { content: String },
    /// 推理内容增量
    ReasoningDelta { content: String },
    /// 工具调用增量（按 index 增量拼接 id/name/arguments，各字段独立到达）
    ToolCallChunk {
        index: usize,
        /// 工具调用 ID（通常在首个 chunk 中到达，部分供应商可能在后续 chunk 到达）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// 工具名称（通常在首个 chunk 中到达，部分供应商可能在后续 chunk 到达）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// 工具参数增量
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args_delta: Option<String>,
    },
    /// 流结束
    Done {
        usage: StreamUsage,
        finish_reason: FinishReason,
    },
}

/// 流式使用统计
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StreamUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_reasoning_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cached_tokens: Option<u32>,
}

/// 完成原因
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
}

/// 流式选项
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    pub temperature: Option<f64>,
    pub tools: Option<Vec<serde_json::Value>>,
    pub tool_choice: Option<serde_json::Value>,
    /// 思考开关（由 ModelConfig 透传，build_request_body 按是否 Some 注入）
    pub thinking_type: Option<ThinkingType>,
    /// 思考强度档位名（用户自定义字符串，透传给服务器）
    pub reasoning_effort: Option<String>,
}

/// 对话请求
#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub system: Option<String>,
}

/// 对话消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl Default for ChatMessage {
    fn default() -> Self {
        Self {
            role: "user".to_string(),
            content: None,
            reasoning: None,
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
        }
    }
}

/// 非流式对话响应
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub reasoning: Option<String>,
    pub tool_calls: Option<Vec<ToolCallData>>,
    pub usage: StreamUsage,
    pub finish_reason: FinishReason,
}

/// 工具调用数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallData {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 流类型别名
pub type BoxStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>;

/// LLM Provider 统一抽象
///
/// 所有 LLM 供应商实现此 trait。
/// stream_chat 返回 BoxStream<StreamEvent>，Engine 层统一消费。
#[async_trait]
pub trait Provider: Send + Sync {
    /// 流式对话，返回增量事件流
    fn stream_chat(
        &self,
        request: ChatRequest,
        model: &str,
        options: StreamOptions,
    ) -> BoxStream<Result<StreamEvent, StreamError>>;

    /// 非流式对话（用于压缩等不需要增量的场景）
    async fn chat(&self, request: ChatRequest, model: &str) -> Result<ChatResponse, StreamError>;
}

/// 流式错误
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("API 请求失败: {0}")]
    ApiError(String),
    #[error("流式响应解析失败: {0}")]
    StreamParseError(String),
    #[error("连接超时")]
    Timeout,
    #[error("认证失败: {0}")]
    AuthError(String),
    #[error("速率限制")]
    RateLimit {
        /// HTTP `retry-after-ms` 响应头（毫秒）
        retry_after_ms: Option<u64>,
        /// HTTP `retry-after` 响应头（秒）
        retry_after_secs: Option<u64>,
    },
    #[error("连接错误: {0}")]
    Connection(String),
    #[error("上下文溢出")]
    ContextOverflow,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_event_text_delta_serializes() {
        let event = StreamEvent::TextDelta {
            content: "Hello".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"text_delta\""));
    }

    #[test]
    fn stream_event_tool_call_chunk_serializes() {
        let event = StreamEvent::ToolCallChunk {
            index: 0,
            id: Some("call_1".to_string()),
            name: Some("read_file".to_string()),
            args_delta: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"tool_call_chunk\""));
        assert!(json.contains("\"id\":\"call_1\""));
        assert!(json.contains("\"name\":\"read_file\""));
        assert!(!json.contains("args_delta"));
    }

    #[test]
    fn stream_usage_default_is_zero() {
        let usage = StreamUsage::default();
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 0);
    }

    #[test]
    fn chat_message_default_role_is_user() {
        let msg = ChatMessage::default();
        assert_eq!(msg.role, "user");
    }

    #[test]
    fn finish_reason_serializes_snake_case() {
        let reason = FinishReason::ToolCalls;
        let json = serde_json::to_string(&reason).unwrap();
        assert!(json.contains("\"tool_calls\""));
    }
}
