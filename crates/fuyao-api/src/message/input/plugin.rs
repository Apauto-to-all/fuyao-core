//! 插件输入事件
//!
//! 插件通过 SendInputFn 钩子发送插件通知（警告、状态等）。
//! 引擎收到后转发为 OutputEvent::Plugin 通知 UI。
//!
//! 本模块还定义插件事件来源的强类型 `PluginEventSource` / `PluginOrigin`（输入输出共享，
//! 纯结构无方向语义，类似 InterruptSource 的共享处理方式）。

use crate::message::EventBase;

/// 插件事件来源（输入输出共享，强类型化）
///
/// 替代原 `PluginData.source: String` 裸字符串，用于 PluginMessage（插件事件）的来源标识。
///
/// 注：此类型与 `user::PluginSource`（用户消息来源 `UserMessageSource::Plugin` 用）语义不同：
/// - `PluginEventSource`：插件**事件**的来源（{origin 内外, name}），用于 PluginMessage
/// - `PluginSource`（user.rs）：插件**注入用户消息**的来源（{name}），用于 UserMessageSource::Plugin
///
/// 两者故意区分命名，不合并。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginEventSource {
    /// 来源方向（core 内置 / 外部开发者）
    pub origin: PluginOrigin,
    /// 插件标识（origin 内自行保证唯一）
    pub name: String,
}

/// 插件来源方向（内外二元区分）
///
/// 开发初期仅区分内外。未来需更细粒度（按子系统 mcp/skill 区分），
/// 扩展本枚举变体即可，不影响 PluginEventSource 结构。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PluginOrigin {
    /// core 内置插件（session_mgr / guard 等）
    Internal,
    /// 外部开发者插件
    External,
}

/// 插件事件 envelope
///
/// 插件通过 SendInputFn 发送，引擎主循环接收后
/// 通过 emit(OutputEvent::Plugin(...)) 通知 UI。
///
/// 注：与 `output::PluginMessage` 字段当前完全一致，但故意独立定义、不共享类型（见方案第八节）。
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
    /// 来源插件（强类型 {origin, name}）
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

    #[test]
    fn plugin_payload_holds_fields() {
        let payload = PluginPayload {
            source: PluginEventSource {
                origin: PluginOrigin::Internal,
                name: "loop_guard".into(),
            },
            event_type: "loop_warn".into(),
            data: None,
            error: None,
            message: Some("检测到循环".into()),
        };
        assert_eq!(payload.source.name, "loop_guard");
        assert_eq!(payload.source.origin, PluginOrigin::Internal);
        assert_eq!(payload.event_type, "loop_warn");
        assert_eq!(payload.message, Some("检测到循环".to_string()));
    }

    #[test]
    fn plugin_message_envelope_holds_base_and_payload() {
        let msg = PluginMessage {
            base: EventBase::default(),
            payload: PluginPayload {
                source: PluginEventSource {
                    origin: PluginOrigin::External,
                    name: "my_reporter".into(),
                },
                event_type: "progress".into(),
                data: Some(serde_json::json!({"percent": 50})),
                error: None,
                message: None,
            },
        };
        assert!(!msg.base.id.is_empty());
        assert_eq!(msg.payload.source.origin, PluginOrigin::External);
        assert_eq!(msg.payload.data.unwrap()["percent"], 50);
    }

    #[test]
    fn plugin_origin_equality() {
        assert_eq!(PluginOrigin::Internal, PluginOrigin::Internal);
        assert_ne!(PluginOrigin::Internal, PluginOrigin::External);
    }
}
