//! 工具结果事件
//!
//! 工具执行完成后，推送此事件通知前端。

use crate::message::EventBase;

/// 工具结果事件 envelope
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolResultMessage {
    /// 事件元信息（id/timestamp）
    pub base: EventBase,
    /// 工具结果载荷
    pub payload: ToolResultPayload,
}

/// 工具结果载荷
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolResultPayload {
    /// 工具调用 ID
    pub tool_call_id: String,
    /// 工具名称
    pub tool_name: String,
    /// 工具结果内容
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    #[test]
    fn tool_result_holds_fields() {
        let msg = ToolResultMessage {
            base: EventBase::default(),
            payload: ToolResultPayload {
                tool_call_id: "call_1".into(),
                tool_name: "get_weather".into(),
                content: "sunny".into(),
            },
        };
        assert_eq!(msg.payload.tool_call_id, "call_1");
        assert_eq!(msg.payload.tool_name, "get_weather");
        assert_eq!(msg.payload.content, "sunny");
    }

    #[test]
    fn tool_result_content_can_be_empty() {
        let msg = ToolResultMessage {
            base: EventBase::default(),
            payload: ToolResultPayload {
                tool_call_id: "call_2".into(),
                tool_name: "noop".into(),
                content: String::new(),
            },
        };
        assert_eq!(msg.payload.content, "");
    }

    #[test]
    fn tool_result_clone_works() {
        let msg = ToolResultMessage {
            base: EventBase::default(),
            payload: ToolResultPayload {
                tool_call_id: "call_1".into(),
                tool_name: "search".into(),
                content: "结果内容".into(),
            },
        };
        let cloned = msg.clone();
        assert_eq!(msg.payload.tool_call_id, cloned.payload.tool_call_id);
        assert_eq!(msg.payload.tool_name, cloned.payload.tool_name);
        assert_eq!(msg.payload.content, cloned.payload.content);
    }

    #[test]
    fn tool_result_timestamp_is_set() {
        let msg = ToolResultMessage {
            base: EventBase::default(),
            payload: ToolResultPayload {
                tool_call_id: "call_1".into(),
                tool_name: "test".into(),
                content: "结果".into(),
            },
        };
        assert!(msg.base.timestamp > 0.0);
    }
}
