//! 输入事件定义
//!
//! 定义 UI 层发送给 Engine 层的所有事件类型。
//! 每个事件对应一个枚举变体，envelope + payload 放在独立文件中管理。
//!
//! - `User`: 用户消息（envelope {base, payload}）
//! - `Interrupt`: 中断信号（envelope {base, payload}）
//! - `Control`: 控制命令消息（envelope {base, payload}，命令本体在 payload.command）

// 子模块：每种事件类型独立文件管理
mod control;
mod interrupt;
mod user_message;

// envelope / payload 在 input 层导出（外部通过 input::UserMessage 等路径访问）
pub use control::{ControlMessage, ControlPayload};
pub use interrupt::{InterruptMessage, InterruptPayload, InterruptSource};
pub use user_message::{
    PluginSource, SystemSource, UserMessage, UserMessageMode, UserMessageSource, UserPayload,
};

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
    /// 控制命令消息：命令主循环在队列消费时机执行（如手动压缩）
    ///
    /// 与用户消息同型排队：入口转化为 output 侧消息后进入 guide / pending
    /// 双队列，mode 决定生效时机。新增控制类功能 = 给
    /// [`ControlCommand`](crate::message::control::ControlCommand) 加变体，不加新输入事件。
    Control(ControlMessage),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;
    use crate::message::control::ControlCommand;

    #[test]
    fn user_event_contains_content_and_mode() {
        let event = InputEvent::User(UserMessage {
            base: EventBase::default(),
            payload: UserPayload {
                content: "测试消息".into(),
                images: vec![],
                mode: UserMessageMode::Guide,
                source: UserMessageSource::User,
                client_message_id: None,
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
    fn control_event_carries_command_and_mode() {
        let event = InputEvent::Control(ControlMessage {
            base: EventBase::default(),
            payload: ControlPayload {
                command: ControlCommand::Compress,
                mode: UserMessageMode::Guide,
                client_message_id: None,
            },
        });
        match &event {
            InputEvent::Control(msg) => {
                assert_eq!(msg.payload.command, ControlCommand::Compress);
                assert_eq!(msg.payload.mode, UserMessageMode::Guide);
            }
            _ => panic!("应为 Control 变体"),
        }
    }

    #[test]
    fn input_event_serde_roundtrip() {
        let event = InputEvent::User(UserMessage {
            base: EventBase::default(),
            payload: UserPayload {
                content: "序列化".into(),
                images: vec![],
                mode: UserMessageMode::Guide,
                source: UserMessageSource::User,
                client_message_id: None,
            },
        });
        let json = serde_json::to_string(&event).expect("序列化失败");
        let de: InputEvent = serde_json::from_str(&json).expect("反序列化失败");
        match de {
            InputEvent::User(msg) => assert_eq!(msg.payload.content, "序列化"),
            _ => panic!("反序列化后应为 User 变体"),
        }
    }

    #[test]
    fn control_event_serde_roundtrip() {
        let event = InputEvent::Control(ControlMessage {
            base: EventBase::default(),
            payload: ControlPayload {
                command: ControlCommand::Compress,
                mode: UserMessageMode::Pending,
                client_message_id: Some("cmd-1".to_string()),
            },
        });
        let json = serde_json::to_string(&event).expect("序列化失败");
        let de: InputEvent = serde_json::from_str(&json).expect("反序列化失败");
        match de {
            InputEvent::Control(msg) => {
                assert_eq!(msg.payload.command, ControlCommand::Compress);
                assert_eq!(msg.payload.mode, UserMessageMode::Pending);
                assert_eq!(msg.payload.client_message_id.as_deref(), Some("cmd-1"));
            }
            _ => panic!("反序列化后应为 Control 变体"),
        }
    }
}
