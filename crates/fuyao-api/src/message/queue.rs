//! 队列条目
//!
//! guide / pending 双队列与 session 入站通道的统一载荷。用户消息与控制命令
//! 消息同型排队：一枚举承载两种条目，排队模式由各自 payload 的 mode 携带。
//! 所有进入 session 的排队消息（含插件注入）共用一条入站通道——
//! 发送顺序即排队顺序。

use super::output::{ControlMessage as OutputControlMessage, UserMessage as OutputUserMessage};

/// 队列条目：guide / pending 双队列的载荷
///
/// 载荷直接复用 output 侧消息类型——入站通道与队列载荷类型完全一致，
/// 复用同一类型避免无意义的拆解/重组（也消除字段丢失风险）。
pub enum QueueEntry {
    /// 用户消息条目（消费时经统一历史管道注入）
    User(OutputUserMessage),
    /// 控制命令条目（消费时就地执行命令本体）
    Control(OutputControlMessage),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;
    use crate::message::output::{ControlPayload, UserPayload};
    use crate::message::{ControlCommand, SystemSource, UserMessageMode, UserMessageSource};

    /// 两类条目各持完整消息本体，模式经 payload 携带
    #[test]
    fn queue_entry_holds_message_payloads() {
        let entry = QueueEntry::User(OutputUserMessage {
            base: EventBase::default(),
            payload: UserPayload {
                content: "你好".to_string(),
                images: vec![],
                mode: UserMessageMode::Guide,
                source: UserMessageSource::System(SystemSource {
                    reason: "test".to_string(),
                }),
                client_message_id: None,
            },
        });
        match entry {
            QueueEntry::User(m) => assert_eq!(m.payload.content, "你好"),
            QueueEntry::Control(_) => panic!("应为 User 条目"),
        }

        let entry = QueueEntry::Control(OutputControlMessage {
            base: EventBase::default(),
            payload: ControlPayload {
                command: ControlCommand::Compress,
                mode: UserMessageMode::Pending,
                client_message_id: None,
                note: None,
            },
        });
        match entry {
            QueueEntry::Control(m) => assert_eq!(m.payload.command, ControlCommand::Compress),
            QueueEntry::User(_) => panic!("应为 Control 条目"),
        }
    }
}
