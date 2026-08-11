//! 非流式响应类型（纯反序列化模型，不依赖 HTTP）
//!
//! OpenAI 兼容 `/chat/completions` **非流式**响应的 serde 模型。仅做结构定义，
//! 由 [`super::OpenAIProvider::chat`] 反序列化后映射为内部的 [`crate::ChatResponse`]。

use serde::Deserialize;

/// 非流式 API 响应
#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionResponse {
    pub choices: Vec<ChatCompletionChoice>,
    #[serde(default)]
    pub usage: Option<ChatCompletionUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionChoice {
    pub message: ChatCompletionMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// 非流式消息（含思考内容）
#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionMessage {
    #[serde(default)]
    pub content: Option<String>,
    /// 思考模式：reasoning_content 字段（DeepSeek / Qwen 等）
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ChatCompletionToolCall>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionToolCall {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<ChatCompletionFunction>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionUsage {
    #[serde(default)]
    pub prompt_tokens: Option<u32>,
    #[serde(default)]
    pub completion_tokens: Option<u32>,
    #[serde(default)]
    pub total_tokens: Option<u32>,
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CompletionTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
