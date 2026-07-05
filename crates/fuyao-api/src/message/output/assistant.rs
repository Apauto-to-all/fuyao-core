//! 助手消息完成事件
//!
//! AI 完整输出一条消息时推送，包含内容、推理、工具调用、用量等全部信息。

use crate::message::EventBase;
use crate::message::output::tool_call::ToolCallPayload;

/// 助手消息完成事件 envelope
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AssistantMessage {
    /// 事件元信息（id/timestamp）
    pub base: EventBase,
    /// 助手消息载荷
    pub payload: AssistantPayload,
}

/// 助手消息载荷
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AssistantPayload {
    /// 消息内容
    pub content: Option<String>,
    /// 推理内容
    pub reasoning: Option<String>,
    /// 工具调用列表（复用 ToolCallPayload，裸 payload 无 envelope）
    pub tool_calls: Option<Vec<ToolCallPayload>>,
    /// 完成原因：stop、tool_calls、length、content_filter
    pub finish_reason: Option<String>,
    /// 输出令牌数
    pub completion_tokens: i64,
    /// 提示词令牌数
    pub prompt_tokens: i64,
    /// 总令牌数
    pub total_tokens: i64,
    /// 推理令牌数
    pub reasoning_tokens: i64,
    /// 缓存命中令牌数
    pub cached_tokens: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    fn make_msg(
        content: Option<&str>,
        tool_calls: Option<Vec<ToolCallPayload>>,
    ) -> AssistantMessage {
        AssistantMessage {
            base: EventBase::default(),
            payload: AssistantPayload {
                content: content.map(|s| s.into()),
                reasoning: None,
                tool_calls,
                finish_reason: Some("stop".into()),
                completion_tokens: 10,
                prompt_tokens: 20,
                total_tokens: 30,
                reasoning_tokens: 0,
                cached_tokens: 0,
            },
        }
    }

    #[test]
    fn assistant_with_content() {
        let msg = make_msg(Some("你好"), None);
        assert_eq!(msg.payload.content.as_deref(), Some("你好"));
        assert!(msg.payload.tool_calls.is_none());
    }

    #[test]
    fn assistant_without_content() {
        let msg = make_msg(None, None);
        assert!(msg.payload.content.is_none());
    }

    #[test]
    fn assistant_with_tool_calls() {
        let msg = make_msg(
            None,
            Some(vec![ToolCallPayload {
                tool_call_id: "call_1".into(),
                tool_name: "search".into(),
                tool_args: serde_json::json!({"q": "rust"}),
            }]),
        );
        assert_eq!(msg.payload.tool_calls.as_ref().unwrap().len(), 1);
        assert_eq!(
            msg.payload.tool_calls.as_ref().unwrap()[0].tool_name,
            "search"
        );
    }

    #[test]
    fn assistant_clone_works() {
        let msg = make_msg(Some("clone测试"), None);
        let cloned = msg.clone();
        assert_eq!(msg.payload.content, cloned.payload.content);
        assert_eq!(msg.payload.reasoning, cloned.payload.reasoning);
        assert!(cloned.payload.tool_calls.is_none());
    }

    #[test]
    fn assistant_timestamp_is_set() {
        let msg = make_msg(Some("时间测试"), None);
        assert!(msg.base.timestamp > 0.0);
    }
}
