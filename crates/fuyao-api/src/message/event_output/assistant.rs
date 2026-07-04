//! 助手消息完成事件
//!
//! AI 完整输出一条消息时推送，包含内容、推理、工具调用、用量等全部信息。

use crate::message::EventBase;
use crate::message::event_output::tool_call::ToolCallData;

/// 助手消息完成数据
#[derive(Debug, Clone, serde::Serialize)]
pub struct AssistantData {
    /// 事件基类（时间戳等公共字段）
    pub base: EventBase,
    /// 消息内容
    pub content: Option<String>,
    /// 推理内容
    pub reasoning: Option<String>,
    /// 工具调用列表
    pub tool_calls: Option<Vec<ToolCallData>>,
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

    fn make_data(content: Option<&str>, tool_calls: Option<Vec<ToolCallData>>) -> AssistantData {
        AssistantData {
            base: EventBase::default(),
            content: content.map(|s| s.into()),
            reasoning: None,
            tool_calls,
            finish_reason: Some("stop".into()),
            completion_tokens: 10,
            prompt_tokens: 20,
            total_tokens: 30,
            reasoning_tokens: 0,
            cached_tokens: 0,
        }
    }

    #[test]
    fn assistant_with_content() {
        let data = make_data(Some("你好"), None);
        assert_eq!(data.content.as_deref(), Some("你好"));
        assert!(data.tool_calls.is_none());
    }

    #[test]
    fn assistant_without_content() {
        let data = make_data(None, None);
        assert!(data.content.is_none());
    }

    #[test]
    fn assistant_with_tool_calls() {
        let data = make_data(
            None,
            Some(vec![ToolCallData {
                base: EventBase::default(),
                tool_call_id: "call_1".into(),
                tool_name: "search".into(),
                tool_args: serde_json::json!({"q": "rust"}),
            }]),
        );
        assert_eq!(data.tool_calls.as_ref().unwrap().len(), 1);
        assert_eq!(data.tool_calls.as_ref().unwrap()[0].tool_name, "search");
    }

    #[test]
    fn assistant_clone_works() {
        let data = make_data(Some("clone测试"), None);
        let cloned = data.clone();
        assert_eq!(data.content, cloned.content);
        assert_eq!(data.reasoning, cloned.reasoning);
        assert!(cloned.tool_calls.is_none());
    }

    #[test]
    fn assistant_timestamp_is_set() {
        let data = make_data(Some("时间测试"), None);
        assert!(data.base.timestamp > 0.0);
    }
}
