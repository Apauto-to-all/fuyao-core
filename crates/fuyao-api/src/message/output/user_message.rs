//! 用户消息输出事件
//!
//! 引擎处理用户输入后发出，CLI 据此渲染用户消息。
//! 确保引擎处理的内容与 CLI 显示的内容一致。
//!
//! 注：与 `input::UserMessage` 字段当前完全一致，但故意独立定义、不共享类型（见方案第八节）。
//! UserMessageMode / UserMessageSource 为纯枚举/结构（无方向语义），定义在 input 侧，输出侧 use 引用。

use crate::message::EventBase;
use crate::message::input::{UserMessageMode, UserMessageSource};

/// 用户消息输出事件 envelope
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UserMessage {
    /// 事件元信息（id/timestamp）
    pub base: EventBase,
    /// 用户消息载荷
    pub payload: UserPayload,
}

/// 用户消息输出载荷
///
/// 与 input::UserPayload 字段一致，但独立定义以便后续拓展输出特有字段。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UserPayload {
    /// 消息文本内容
    pub content: String,
    /// 消息模式
    pub mode: UserMessageMode,
    /// 消息来源
    pub source: UserMessageSource,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    #[test]
    fn user_message_holds_fields() {
        let msg = UserMessage {
            base: EventBase::default(),
            payload: UserPayload {
                content: "你好".into(),
                mode: UserMessageMode::Guide,
                source: UserMessageSource::User,
            },
        };
        assert_eq!(msg.payload.content, "你好");
        assert_eq!(msg.payload.mode, UserMessageMode::Guide);
        assert_eq!(msg.payload.source, UserMessageSource::User);
    }

    #[test]
    fn user_message_clone_works() {
        let msg = UserMessage {
            base: EventBase::default(),
            payload: UserPayload {
                content: "clone测试".into(),
                mode: UserMessageMode::Pending,
                source: UserMessageSource::User,
            },
        };
        let cloned = msg.clone();
        assert_eq!(msg.payload.content, cloned.payload.content);
        assert_eq!(msg.payload.mode, cloned.payload.mode);
        assert_eq!(msg.payload.source, cloned.payload.source);
    }
}
