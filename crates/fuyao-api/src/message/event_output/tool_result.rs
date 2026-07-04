//! 工具结果事件
//!
//! 工具执行完成后，推送此事件通知前端。

use crate::message::EventBase;

/// 工具结果数据
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolResultData {
    /// 事件基类（时间戳等公共字段）
    pub base: EventBase,
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
        let tr = ToolResultData {
            base: EventBase::default(),
            tool_call_id: "call_1".into(),
            tool_name: "get_weather".into(),
            content: "sunny".into(),
        };
        assert_eq!(tr.tool_call_id, "call_1");
        assert_eq!(tr.tool_name, "get_weather");
        assert_eq!(tr.content, "sunny");
    }

    #[test]
    fn tool_result_content_can_be_empty() {
        let tr = ToolResultData {
            base: EventBase::default(),
            tool_call_id: "call_2".into(),
            tool_name: "noop".into(),
            content: String::new(),
        };
        assert_eq!(tr.content, "");
    }

    #[test]
    fn tool_result_clone_works() {
        let tr = ToolResultData {
            base: EventBase::default(),
            tool_call_id: "call_1".into(),
            tool_name: "search".into(),
            content: "结果内容".into(),
        };
        let cloned = tr.clone();
        assert_eq!(tr.tool_call_id, cloned.tool_call_id);
        assert_eq!(tr.tool_name, cloned.tool_name);
        assert_eq!(tr.content, cloned.content);
    }

    #[test]
    fn tool_result_timestamp_is_set() {
        let tr = ToolResultData {
            base: EventBase::default(),
            tool_call_id: "call_1".into(),
            tool_name: "test".into(),
            content: "结果".into(),
        };
        assert!(tr.base.timestamp > 0.0);
    }
}
