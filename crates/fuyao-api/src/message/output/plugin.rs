//! 插件事件
//!
//! 注：与 `input::PluginMessage` 字段当前完全一致，但故意独立定义、不共享类型（见方案第八节）。
//! PluginEventSource 为插件事件通用来源类型（无方向语义），
//! 定义在 input 侧，输出侧 use 引用（类似 InterruptSource 的共享处理）。

use crate::message::EventBase;
use crate::message::input::PluginEventSource;

/// 插件事件 envelope
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginMessage {
    /// 事件元信息（id/timestamp）
    pub base: EventBase,
    /// 插件事件载荷
    pub payload: PluginPayload,
}

/// 插件事件载荷
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginPayload {
    /// 来源插件（强类型 {name}）
    pub source: PluginEventSource,
    /// 事件类型（插件自定义字符串标识）
    pub event_type: String,
    /// 事件数据（任意 JSON，core 不解析透传）
    pub data: Option<serde_json::Value>,
    /// 错误信息
    pub error: Option<String>,
    /// 提醒信息
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;
    use crate::message::input::PluginEventSource;

    #[test]
    fn plugin_holds_source_and_type() {
        let msg = PluginMessage {
            base: EventBase::default(),
            payload: PluginPayload {
                source: PluginEventSource {
                    name: "session_mgr".into(),
                },
                event_type: "compressed".into(),
                data: None,
                error: None,
                message: None,
            },
        };
        assert_eq!(msg.payload.source.name, "session_mgr");
        assert_eq!(msg.payload.event_type, "compressed");
    }

    #[test]
    fn plugin_with_data() {
        let msg = PluginMessage {
            base: EventBase::default(),
            payload: PluginPayload {
                source: PluginEventSource {
                    name: "test".into(),
                },
                event_type: "custom".into(),
                data: Some(serde_json::json!({"key": "value"})),
                error: None,
                message: None,
            },
        };
        assert_eq!(msg.payload.data.as_ref().unwrap()["key"], "value");
    }

    #[test]
    fn plugin_with_error() {
        let msg = PluginMessage {
            base: EventBase::default(),
            payload: PluginPayload {
                source: PluginEventSource {
                    name: "test".into(),
                },
                event_type: "error".into(),
                data: None,
                error: Some("失败了".into()),
                message: None,
            },
        };
        assert_eq!(msg.payload.error.as_deref(), Some("失败了"));
    }

    #[test]
    fn plugin_with_message() {
        let msg = PluginMessage {
            base: EventBase::default(),
            payload: PluginPayload {
                source: PluginEventSource {
                    name: "test".into(),
                },
                event_type: "info".into(),
                data: None,
                error: None,
                message: Some("操作完成".into()),
            },
        };
        assert_eq!(msg.payload.message.as_deref(), Some("操作完成"));
    }

    #[test]
    fn plugin_clone_works() {
        let msg = PluginMessage {
            base: EventBase::default(),
            payload: PluginPayload {
                source: PluginEventSource {
                    name: "test".into(),
                },
                event_type: "info".into(),
                data: Some(serde_json::json!({"n": 1})),
                error: None,
                message: Some("done".into()),
            },
        };
        let cloned = msg.clone();
        assert_eq!(msg.payload.source.name, cloned.payload.source.name);
        assert_eq!(msg.payload.event_type, cloned.payload.event_type);
        assert_eq!(msg.payload.data, cloned.payload.data);
    }

    #[test]
    fn plugin_timestamp_is_set() {
        let msg = PluginMessage {
            base: EventBase::default(),
            payload: PluginPayload {
                source: PluginEventSource {
                    name: "test".into(),
                },
                event_type: "info".into(),
                data: None,
                error: None,
                message: None,
            },
        };
        assert!(msg.base.timestamp > 0.0);
    }
}
