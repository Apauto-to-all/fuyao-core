//! 工具调用事件
//!
//! 当 LLM 决定调用工具时，推送此事件通知前端。
//!
//! `ToolCallPayload` 同时复用于 `AssistantPayload.tool_calls` 数组元素（裸 payload，无 envelope）——
//! 一个载荷类型覆盖「独立事件」与「数组子元素」两个场景。

use crate::message::EventBase;

/// 工具调用事件 envelope
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCallMessage {
    /// 事件元信息（id/timestamp）
    pub base: EventBase,
    /// 工具调用载荷
    pub payload: ToolCallPayload,
}

/// 工具调用载荷（无 base，跨场景复用）
///
/// ① 独立 `ToolCallMessage.payload`（base 由 envelope 提供）
/// ② `AssistantPayload.tool_calls` 数组元素（裸 payload，无 envelope）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCallPayload {
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
        let msg = ToolCallMessage {
            base: EventBase::default(),
            payload: ToolCallPayload {
                tool_call_id: "call_1".into(),
                tool_name: "get_weather".into(),
                tool_args: serde_json::json!({"city": "北京"}),
            },
        };
        assert_eq!(msg.payload.tool_call_id, "call_1");
        assert_eq!(msg.payload.tool_name, "get_weather");
        assert_eq!(msg.payload.tool_args["city"], "北京");
    }

    #[test]
    fn tool_call_args_empty_object() {
        let msg = ToolCallMessage {
            base: EventBase::default(),
            payload: ToolCallPayload {
                tool_call_id: "call_2".into(),
                tool_name: "noop".into(),
                tool_args: serde_json::json!({}),
            },
        };
        assert_eq!(msg.payload.tool_args, serde_json::json!({}));
    }

    #[test]
    fn tool_call_clone_works() {
        let msg = ToolCallMessage {
            base: EventBase::default(),
            payload: ToolCallPayload {
                tool_call_id: "call_1".into(),
                tool_name: "search".into(),
                tool_args: serde_json::json!({"q": "rust"}),
            },
        };
        let cloned = msg.clone();
        assert_eq!(msg.payload.tool_call_id, cloned.payload.tool_call_id);
        assert_eq!(msg.payload.tool_name, cloned.payload.tool_name);
        assert_eq!(msg.payload.tool_args, cloned.payload.tool_args);
    }

    #[test]
    fn tool_call_timestamp_is_set() {
        let msg = ToolCallMessage {
            base: EventBase::default(),
            payload: ToolCallPayload {
                tool_call_id: "call_1".into(),
                tool_name: "test".into(),
                tool_args: serde_json::json!({}),
            },
        };
        assert!(msg.base.timestamp > 0.0);
    }

    #[test]
    fn tool_call_payload_reuse_bare() {
        // 验证裸 payload 可独立构造（供 AssistantPayload.tool_calls 子元素复用）
        let payload = ToolCallPayload {
            tool_call_id: "call_1".into(),
            tool_name: "search".into(),
            tool_args: serde_json::json!({"q": "rust"}),
        };
        assert_eq!(payload.tool_name, "search");
    }
}
