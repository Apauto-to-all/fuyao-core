//! 控制命令输入消息
//!
//! 控制命令是一种消息：与用户消息同型排队、同序消费。经
//! [`crate::message::input::InputEvent::Control`] 送入引擎入口，在入口转化为
//! output 侧消息后随入站通道进入 guide / pending 双队列，在队列消费时机生效。
//! `mode` 与用户消息同语义——发送方据此自选生效时机：
//! - `Guide`：下一消费时机（工具批完成后 / 最终回复后）生效
//! - `Pending`：最终回复后才生效
//!
//! 新增控制类功能 = 给 [`ControlCommand`](crate::message::control::ControlCommand) 加变体，
//! 不加新输入消息、不开新通道。

use crate::message::EventBase;
use crate::message::control::ControlCommand;
use crate::message::input::UserMessageMode;

/// 控制命令消息 envelope
///
/// 由 [`crate::message::input::InputEvent::Control`] 携带。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ControlMessage {
    /// 事件元信息（seq/timestamp）
    pub base: EventBase,
    /// 控制命令载荷
    pub payload: ControlPayload,
}

/// 控制命令载荷
///
/// 命令本体 + 生效时机 + 客户端标识 + 可选附言。触发参数（如压缩的模型、上下文长度）
/// 由引擎在执行时按 session 配置现解析，不随消息携带；附言是发送方的意图内容，
/// 是否消费由各命令自决（多数命令视作一段提示词交给 AI 自行理解）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ControlPayload {
    /// 要执行的命令
    pub command: ControlCommand,
    /// 生效时机（Guide：下一消费时机；Pending：最终回复后）
    pub mode: UserMessageMode,
    /// 客户端命令标识：由发送方生成、会话内唯一，仅用于队列管理
    /// （撤销排队中尚未生效的命令 / 消费回显配对）；随消费回显原样携带，不落库
    #[serde(default)]
    pub client_message_id: Option<String>,
    /// 控制命令附言：发送方随命令附带的可选自由文本（如手动压缩时指定摘要
    /// 的侧重要求）。随消费回显原样携带，不落库；新增命令禁止默认把附言设为
    /// 必填——确需必填且缺失即无法实现功能时，由该命令的消费点执行体校验并
    /// 发错误事件，引擎入口不做必填校验
    #[serde(default)]
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    /// 控制消息携带命令本体、生效时机、附言
    #[test]
    fn control_message_holds_command_mode_and_note() {
        let msg = ControlMessage {
            base: EventBase::default(),
            payload: ControlPayload {
                command: ControlCommand::Compress,
                mode: UserMessageMode::Pending,
                client_message_id: Some("cmd-1".to_string()),
                note: Some("重点保留文件路径".to_string()),
            },
        };
        assert!(msg.base.seq.is_none());
        assert_eq!(msg.payload.command, ControlCommand::Compress);
        assert_eq!(msg.payload.mode, UserMessageMode::Pending);
        assert_eq!(msg.payload.client_message_id.as_deref(), Some("cmd-1"));
        assert_eq!(msg.payload.note.as_deref(), Some("重点保留文件路径"));
    }

    /// 控制消息附言 serde 往返：携带与缺省两形态（缺省反序列化为 None）
    #[test]
    fn control_message_note_serde_roundtrip() {
        let carried = ControlMessage {
            base: EventBase::default(),
            payload: ControlPayload {
                command: ControlCommand::Compress,
                mode: UserMessageMode::Guide,
                client_message_id: None,
                note: Some("侧重错误堆栈".to_string()),
            },
        };
        let json = serde_json::to_string(&carried).expect("序列化失败");
        let de: ControlMessage = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(de.payload.note.as_deref(), Some("侧重错误堆栈"));

        let absent = ControlMessage {
            base: EventBase::default(),
            payload: ControlPayload {
                command: ControlCommand::Compress,
                mode: UserMessageMode::Guide,
                client_message_id: None,
                note: None,
            },
        };
        let json = serde_json::to_string(&absent).expect("序列化失败");
        let de: ControlMessage = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(de.payload.note, None, "未携带附言应反序列化为 None");
    }

    /// 控制消息 serde 往返（过 IPC）
    #[test]
    fn control_message_serde_roundtrip() {
        let msg = ControlMessage {
            base: EventBase::default(),
            payload: ControlPayload {
                command: ControlCommand::Compress,
                mode: UserMessageMode::Guide,
                client_message_id: None,
                note: None,
            },
        };
        let json = serde_json::to_string(&msg).expect("序列化失败");
        let de: ControlMessage = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(de.payload.command, ControlCommand::Compress);
        assert_eq!(de.payload.mode, UserMessageMode::Guide);
        assert_eq!(de.payload.client_message_id, None);
    }
}
