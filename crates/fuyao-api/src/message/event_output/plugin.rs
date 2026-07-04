//! 插件事件
//!
//! 字段与 event_input::PluginData 保持一致，
//! 转换时通过 From<event_input::PluginData> 确保字段同步，避免手动构造导致不一致。

use crate::message::EventBase;

/// 插件事件数据
#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginData {
    /// 事件基类（时间戳等公共字段）
    pub base: EventBase,
    /// 来源插件名称
    pub source: String,
    /// 事件类型
    pub event_type: String,
    /// 事件数据
    pub data: Option<serde_json::Value>,
    /// 错误信息
    pub error: Option<String>,
    /// 提醒信息
    pub message: Option<String>,
}

/// 输入插件事件 → 输出插件事件
///
/// 确保转换时字段完全一致，避免手动构造导致字段不同步。
impl From<crate::message::event_input::PluginData> for PluginData {
    fn from(input: crate::message::event_input::PluginData) -> Self {
        Self {
            base: input.base,
            source: input.source,
            event_type: input.event_type,
            data: input.data,
            error: input.error,
            message: input.message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    #[test]
    fn plugin_holds_source_and_type() {
        let p = PluginData {
            base: EventBase::default(),
            source: "session_mgr".into(),
            event_type: "compressed".into(),
            data: None,
            error: None,
            message: None,
        };
        assert_eq!(p.source, "session_mgr");
        assert_eq!(p.event_type, "compressed");
    }

    #[test]
    fn plugin_with_data() {
        let p = PluginData {
            base: EventBase::default(),
            source: "test".into(),
            event_type: "custom".into(),
            data: Some(serde_json::json!({"key": "value"})),
            error: None,
            message: None,
        };
        assert_eq!(p.data.as_ref().unwrap()["key"], "value");
    }

    #[test]
    fn plugin_with_error() {
        let p = PluginData {
            base: EventBase::default(),
            source: "test".into(),
            event_type: "error".into(),
            data: None,
            error: Some("失败了".into()),
            message: None,
        };
        assert_eq!(p.error.as_deref(), Some("失败了"));
    }

    #[test]
    fn plugin_with_message() {
        let p = PluginData {
            base: EventBase::default(),
            source: "test".into(),
            event_type: "info".into(),
            data: None,
            error: None,
            message: Some("操作完成".into()),
        };
        assert_eq!(p.message.as_deref(), Some("操作完成"));
    }

    #[test]
    fn plugin_clone_works() {
        let p = PluginData {
            base: EventBase::default(),
            source: "test".into(),
            event_type: "info".into(),
            data: Some(serde_json::json!({"n": 1})),
            error: None,
            message: Some("done".into()),
        };
        let cloned = p.clone();
        assert_eq!(p.source, cloned.source);
        assert_eq!(p.event_type, cloned.event_type);
        assert_eq!(p.data, cloned.data);
    }

    #[test]
    fn plugin_timestamp_is_set() {
        let p = PluginData {
            base: EventBase::default(),
            source: "test".into(),
            event_type: "info".into(),
            data: None,
            error: None,
            message: None,
        };
        assert!(p.base.timestamp > 0.0);
    }
}
