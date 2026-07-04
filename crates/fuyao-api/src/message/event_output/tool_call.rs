//! 工具调用事件
//!
//! 当 LLM 决定调用工具时，推送此事件通知前端。

use crate::message::EventBase;

/// 工具调用数据
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolCallData {
    /// 事件基类（时间戳等公共字段）
    pub base: EventBase,
    /// 工具调用 ID
    pub tool_call_id: String,
    /// 工具名称
    pub tool_name: String,
    /// 工具参数
    pub tool_args: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    #[test]
    fn tool_call_holds_fields() {
        let tc = ToolCallData {
            base: EventBase::default(),
            tool_call_id: "call_1".into(),
            tool_name: "get_weather".into(),
            tool_args: serde_json::json!({"city": "北京"}),
        };
        assert_eq!(tc.tool_call_id, "call_1");
        assert_eq!(tc.tool_name, "get_weather");
        assert_eq!(tc.tool_args["city"], "北京");
    }

    #[test]
    fn tool_call_args_empty_object() {
        let tc = ToolCallData {
            base: EventBase::default(),
            tool_call_id: "call_2".into(),
            tool_name: "noop".into(),
            tool_args: serde_json::json!({}),
        };
        assert_eq!(tc.tool_args, serde_json::json!({}));
    }

    #[test]
    fn tool_call_clone_works() {
        let tc = ToolCallData {
            base: EventBase::default(),
            tool_call_id: "call_1".into(),
            tool_name: "search".into(),
            tool_args: serde_json::json!({"q": "rust"}),
        };
        let cloned = tc.clone();
        assert_eq!(tc.tool_call_id, cloned.tool_call_id);
        assert_eq!(tc.tool_name, cloned.tool_name);
        assert_eq!(tc.tool_args, cloned.tool_args);
    }

    #[test]
    fn tool_call_timestamp_is_set() {
        let tc = ToolCallData {
            base: EventBase::default(),
            tool_call_id: "call_1".into(),
            tool_name: "test".into(),
            tool_args: serde_json::json!({}),
        };
        assert!(tc.base.timestamp > 0.0);
    }
}
