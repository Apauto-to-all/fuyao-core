//! 输入事件定义
//!
//! 定义 UI 层发送给 Engine 层的所有事件类型。
//! 每个事件对应一个枚举变体，数据放在独立文件中管理。
//!
//! - `user`: 用户消息（含 content、mode）
//! - `interrupt`: 中断信号
//! - `Shutdown`: 关闭引擎（无数据）

// 子模块：每种事件类型独立文件
mod interrupt;
mod plugin;
mod user;

// 公开子模块中的数据类型
pub use interrupt::{InterruptData, InterruptSource};
pub use plugin::PluginData;
pub use user::{PluginSource, SystemSource, UserData, UserMessageMode, UserMessageSource};

/// 输入事件（UI → Engine）
#[derive(Debug, Clone)]
pub enum InputEvent {
    /// 用户消息：用户在界面中发送的消息
    User(UserData),
    /// 中断：用户取消或中断当前操作
    Interrupt(InterruptData),
    /// 关闭引擎：用户退出应用或关闭引擎
    Shutdown,
    /// 插件通知：插件通过 SendInputFn 发送的通知（警告、状态等）
    /// 引擎收到后转发为 OutputEvent::Plugin 通知 UI
    Plugin(PluginData),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    #[test]
    fn user_event_contains_content_and_mode() {
        let event = InputEvent::User(UserData {
            base: EventBase::default(),
            content: "测试消息".into(),
            mode: UserMessageMode::Guide,
            source: UserMessageSource::User,
        });
        match &event {
            InputEvent::User(data) => {
                assert_eq!(data.content, "测试消息");
                assert_eq!(data.mode, UserMessageMode::Guide);
                assert_eq!(data.source, UserMessageSource::User);
            }
            _ => panic!("应为 User 变体"),
        }
        let _ = event; // 无序列化需求，确保 Clone 可用
    }

    #[test]
    fn interrupt_event_contains_reason() {
        let event = InputEvent::Interrupt(InterruptData {
            base: EventBase::default(),
            reason: "用户取消".into(),
            source: InterruptSource::User,
        });
        match &event {
            InputEvent::Interrupt(data) => {
                assert_eq!(data.reason, "用户取消");
                assert_eq!(data.source, InterruptSource::User);
            }
            _ => panic!("应为 Interrupt 变体"),
        }
    }

    #[test]
    fn shutdown_event_unit_variant() {
        let event = InputEvent::Shutdown;
        assert!(matches!(event, InputEvent::Shutdown));
    }

    #[test]
    fn plugin_event_contains_source_and_type() {
        let event = InputEvent::Plugin(PluginData {
            base: EventBase::default(),
            source: "loop_guard".to_string(),
            event_type: "loop_warn".to_string(),
            data: None,
            error: None,
            message: Some("检测到循环".to_string()),
        });
        match &event {
            InputEvent::Plugin(data) => {
                assert_eq!(data.source, "loop_guard");
                assert_eq!(data.event_type, "loop_warn");
                assert_eq!(data.message, Some("检测到循环".to_string()));
            }
            _ => panic!("应为 Plugin 变体"),
        }
    }
}
