//! 输入事件定义
//!
//! 定义 UI 层发送给 Engine 层的所有事件类型。
//! 每个事件对应一个枚举变体，envelope + payload 放在独立文件中管理。
//!
//! - `User`: 用户消息（envelope {base, payload}）
//! - `Interrupt`: 中断信号（envelope {base, payload}）
//! - `Shutdown`: 关闭引擎（envelope {base}，无 payload）
//! - `Plugin`: 插件通知（envelope {base, payload}）

// 子模块：每种事件类型独立文件
mod interrupt;
mod plugin;
mod user;

// envelope / payload 在 input 层导出（外部通过 input::UserMessage 等路径访问）
pub use interrupt::{InterruptMessage, InterruptPayload, InterruptSource};
pub use plugin::{PluginEventSource, PluginMessage, PluginPayload};
pub use user::{
    PluginSource, SystemSource, UserMessage, UserMessageMode, UserMessageSource, UserPayload,
};

use crate::message::EventBase;

/// 关闭引擎事件 envelope（无 payload，仅 base）
///
/// 用户退出应用或关闭引擎时发送。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ShutdownMessage {
    /// 事件元信息（id/timestamp）
    pub base: EventBase,
}

/// 输入事件（UI → Engine）
///
/// envelope 每变体自带 base（在 envelope struct 内），enum 保持穷尽匹配。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum InputEvent {
    /// 用户消息：用户在界面中发送的消息
    User(UserMessage),
    /// 中断：用户取消或中断当前操作
    Interrupt(InterruptMessage),
    /// 关闭引擎：用户退出应用或关闭引擎
    Shutdown(ShutdownMessage),
    /// 插件通知：插件通过 SendInputFn 发送的通知（警告、状态等）
    /// 引擎收到后转发为 OutputEvent::Plugin 通知 UI
    Plugin(PluginMessage),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    #[test]
    fn user_event_contains_content_and_mode() {
        let event = InputEvent::User(UserMessage {
            base: EventBase::default(),
            payload: UserPayload {
                content: "测试消息".into(),
                mode: UserMessageMode::Guide,
                source: UserMessageSource::User,
            },
        });
        match &event {
            InputEvent::User(msg) => {
                assert_eq!(msg.payload.content, "测试消息");
                assert_eq!(msg.payload.mode, UserMessageMode::Guide);
                assert_eq!(msg.payload.source, UserMessageSource::User);
            }
            _ => panic!("应为 User 变体"),
        }
        let _ = event; // 确保 Clone 可用
    }

    #[test]
    fn interrupt_event_contains_reason() {
        let event = InputEvent::Interrupt(InterruptMessage {
            base: EventBase::default(),
            payload: InterruptPayload {
                reason: "用户取消".into(),
                source: InterruptSource::User,
            },
        });
        match &event {
            InputEvent::Interrupt(msg) => {
                assert_eq!(msg.payload.reason, "用户取消");
                assert_eq!(msg.payload.source, InterruptSource::User);
            }
            _ => panic!("应为 Interrupt 变体"),
        }
    }

    #[test]
    fn shutdown_event_has_base_only() {
        let event = InputEvent::Shutdown(ShutdownMessage {
            base: EventBase::default(),
        });
        match &event {
            InputEvent::Shutdown(msg) => assert!(msg.base.timestamp > 0.0),
            _ => panic!("应为 Shutdown 变体"),
        }
    }

    #[test]
    fn plugin_event_contains_source_and_type() {
        let event = InputEvent::Plugin(PluginMessage {
            base: EventBase::default(),
            payload: PluginPayload {
                source: PluginEventSource {
                    name: "loop_guard".into(),
                },
                event_type: "loop_warn".into(),
                data: None,
                error: None,
                message: Some("检测到循环".into()),
            },
        });
        match &event {
            InputEvent::Plugin(msg) => {
                assert_eq!(msg.payload.source.name, "loop_guard");
                assert_eq!(msg.payload.event_type, "loop_warn");
                assert_eq!(msg.payload.message, Some("检测到循环".to_string()));
            }
            _ => panic!("应为 Plugin 变体"),
        }
    }

    #[test]
    fn input_event_serde_roundtrip() {
        let event = InputEvent::User(UserMessage {
            base: EventBase::default(),
            payload: UserPayload {
                content: "序列化".into(),
                mode: UserMessageMode::Guide,
                source: UserMessageSource::User,
            },
        });
        let json = serde_json::to_string(&event).expect("序列化失败");
        let de: InputEvent = serde_json::from_str(&json).expect("反序列化失败");
        match de {
            InputEvent::User(msg) => assert_eq!(msg.payload.content, "序列化"),
            _ => panic!("反序列化后应为 User 变体"),
        }
    }
}
