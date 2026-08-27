//! 控制命令消息（输出侧）
//!
//! 引擎内核流转的控制命令消息：入口（`Engine::send`）把 input 侧
//! `ControlMessage` 转化为本类型，此后入站通道、guide / pending 双队列、
//! 消费全程只认 output 侧。消费时刻经 [`crate::message::OutputEvent`] 的
//! `Control` 变体回显对外——前端据此得知该命令已被消费并即将生效；
//! 命令的执行产物（如 Compression 事件）照常走输出事件流。
//!
//! 注：与 `input::ControlMessage` 字段完全一致，但故意独立定义、不共享类型，
//! 与 UserMessage 的输入输出双侧惯例一致。
//! `ControlCommand` / `UserMessageMode` 为纯枚举（无方向语义），
//! 定义在 crate 根 / input 侧，本模块 use 引用。

use crate::message::EventBase;
use crate::message::control::ControlCommand;
use crate::message::input::UserMessageMode;

/// 控制命令消息 envelope（输出侧）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ControlMessage {
    /// 事件元信息（seq/timestamp）
    pub base: EventBase,
    /// 控制命令载荷
    pub payload: ControlPayload,
}

/// 控制命令载荷（输出侧）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ControlPayload {
    /// 要执行的命令
    pub command: ControlCommand,
    /// 生效时机（Guide：下一消费时机；Pending：最终回复后）
    pub mode: UserMessageMode,
    /// 客户端命令标识：由发送方生成、会话内唯一，仅用于队列管理
    /// （撤销排队中尚未生效的命令 / 消费回显配对），不落库
    #[serde(default)]
    pub client_message_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    /// 输出侧控制消息字段完整（命令 / 时机 / 标识）
    #[test]
    fn output_control_message_holds_fields() {
        let msg = ControlMessage {
            base: EventBase::default(),
            payload: ControlPayload {
                command: ControlCommand::Compress,
                mode: UserMessageMode::Guide,
                client_message_id: Some("cmd-9".to_string()),
            },
        };
        assert_eq!(msg.payload.command, ControlCommand::Compress);
        assert_eq!(msg.payload.mode, UserMessageMode::Guide);
        assert_eq!(msg.payload.client_message_id.as_deref(), Some("cmd-9"));
    }

    /// 输出侧控制消息 serde 往返
    #[test]
    fn output_control_message_serde_roundtrip() {
        let msg = ControlMessage {
            base: EventBase::default(),
            payload: ControlPayload {
                command: ControlCommand::Compress,
                mode: UserMessageMode::Pending,
                client_message_id: None,
            },
        };
        let json = serde_json::to_string(&msg).expect("序列化失败");
        let de: ControlMessage = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(de.payload.command, ControlCommand::Compress);
        assert_eq!(de.payload.mode, UserMessageMode::Pending);
    }
}
