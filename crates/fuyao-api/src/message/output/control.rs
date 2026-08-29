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
///
/// 附言是发送方的意图内容，是否消费由各命令自决；触发参数（如压缩的模型、
/// 上下文长度）仍由引擎在执行时按 session 配置现解析，不随消息携带。
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
    /// 控制命令附言：发送方随命令附带的可选自由文本（如手动压缩时指定摘要
    /// 的侧重要求）。随消费回显原样携带，不落库；新增命令禁止默认把附言设为
    /// 必填——确需必填且缺失即无法实现功能时，由该命令的消费点执行体校验并
    /// 发错误事件，引擎入口不做必填校验
    #[serde(default)]
    pub note: Option<String>,
}

impl ControlMessage {
    /// 消费回显事件：命令条目被消费时，先把消息本体以 `Control` 变体对外广播
    ///
    /// 前端据此得知该命令已被消费并即将生效，client_message_id 随回显原样携带
    /// （供排队项配对）。回显照常过 dispatch 管道——可被拦截钩子改写或阻止，
    /// 但那只影响本次回显的对外可见性，命令本体不受影响。消息整体包进事件，
    /// [`ControlCommand`](crate::message::control::ControlCommand) 加变体不需要动本方法。
    pub fn into_echo_event(self) -> crate::message::OutputEvent {
        crate::message::OutputEvent::Control(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    /// 输出侧控制消息字段完整（命令 / 时机 / 标识 / 附言）
    #[test]
    fn output_control_message_holds_fields() {
        let msg = ControlMessage {
            base: EventBase::default(),
            payload: ControlPayload {
                command: ControlCommand::Compress,
                mode: UserMessageMode::Guide,
                client_message_id: Some("cmd-9".to_string()),
                note: Some("侧重未完成的任务".to_string()),
            },
        };
        assert_eq!(msg.payload.command, ControlCommand::Compress);
        assert_eq!(msg.payload.mode, UserMessageMode::Guide);
        assert_eq!(msg.payload.client_message_id.as_deref(), Some("cmd-9"));
        assert_eq!(msg.payload.note.as_deref(), Some("侧重未完成的任务"));
    }

    /// 输出侧附言 serde 往返：携带与缺省两形态（缺省反序列化为 None）
    #[test]
    fn output_control_message_note_serde_roundtrip() {
        let carried = ControlMessage {
            base: EventBase::default(),
            payload: ControlPayload {
                command: ControlCommand::Compress,
                mode: UserMessageMode::Pending,
                client_message_id: None,
                note: Some("保留关键决策".to_string()),
            },
        };
        let json = serde_json::to_string(&carried).expect("序列化失败");
        let de: ControlMessage = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(de.payload.note.as_deref(), Some("保留关键决策"));

        let absent = ControlMessage {
            base: EventBase::default(),
            payload: ControlPayload {
                command: ControlCommand::Compress,
                mode: UserMessageMode::Pending,
                client_message_id: None,
                note: None,
            },
        };
        let json = serde_json::to_string(&absent).expect("序列化失败");
        let de: ControlMessage = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(de.payload.note, None, "未携带附言应反序列化为 None");
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
                note: None,
            },
        };
        let json = serde_json::to_string(&msg).expect("序列化失败");
        let de: ControlMessage = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(de.payload.command, ControlCommand::Compress);
        assert_eq!(de.payload.mode, UserMessageMode::Pending);
    }

    /// 回显事件构造：消息本体（base + payload 全字段）原样包进 Control 变体
    #[test]
    fn output_control_message_into_echo_event_wraps_whole_message() {
        let msg = ControlMessage {
            base: EventBase {
                seq: Some(9),
                timestamp: 42.0,
                session_id: Some("sess-9".to_string()),
            },
            payload: ControlPayload {
                command: ControlCommand::Compress,
                mode: UserMessageMode::Guide,
                client_message_id: Some("cmd-9".to_string()),
                note: Some("侧重错误堆栈".to_string()),
            },
        };
        match msg.into_echo_event() {
            crate::message::OutputEvent::Control(m) => {
                assert_eq!(m.base.seq, Some(9));
                assert_eq!(m.base.timestamp, 42.0);
                assert_eq!(m.base.session_id.as_deref(), Some("sess-9"));
                assert_eq!(m.payload.command, ControlCommand::Compress);
                assert_eq!(m.payload.mode, UserMessageMode::Guide);
                assert_eq!(m.payload.client_message_id.as_deref(), Some("cmd-9"));
                assert_eq!(m.payload.note.as_deref(), Some("侧重错误堆栈"));
            }
            _ => panic!("回显事件应为 Control 变体"),
        }
    }
}
