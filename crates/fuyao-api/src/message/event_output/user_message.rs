//! 用户消息输出事件
//!
//! 引擎处理用户输入后发出，CLI 据此渲染用户消息。
//! 确保引擎处理的内容与 CLI 显示的内容一致。
//!
//! 字段与 event_input::UserData 保持一致，
//! 转换时通过 From<UserData> 确保字段同步，避免手动构造导致不一致。

use crate::message::EventBase;
use crate::message::event_input::{UserMessageMode, UserMessageSource};

/// 用户消息输出数据
///
/// 与 event_input::UserData 字段一致，但独立定义以便后续拓展输出特有字段。
/// 通过 `From<event_input::UserData>` 确保输入→输出转换时字段同步。
#[derive(Debug, Clone, serde::Serialize)]
pub struct UserMessageData {
    /// 事件基类（时间戳等公共字段）
    pub base: EventBase,
    /// 消息文本内容
    pub content: String,
    /// 消息模式
    pub mode: UserMessageMode,
    /// 消息来源
    pub source: UserMessageSource,
}

/// 输入用户消息 → 输出用户消息
///
/// 确保转换时字段完全一致，避免手动构造导致字段不同步。
/// 后续如果输出需要拓展字段，在此处添加默认值即可。
impl From<crate::message::event_input::UserData> for UserMessageData {
    fn from(input: crate::message::event_input::UserData) -> Self {
        Self {
            base: input.base,
            content: input.content,
            mode: input.mode,
            source: input.source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    #[test]
    fn user_message_data_holds_fields() {
        let data = UserMessageData {
            base: EventBase::default(),
            content: "你好".into(),
            mode: UserMessageMode::Guide,
            source: UserMessageSource::User,
        };
        assert_eq!(data.content, "你好");
        assert_eq!(data.mode, UserMessageMode::Guide);
        assert_eq!(data.source, UserMessageSource::User);
    }

    #[test]
    fn user_message_data_clone_works() {
        let data = UserMessageData {
            base: EventBase::default(),
            content: "clone测试".into(),
            mode: UserMessageMode::Pending,
            source: UserMessageSource::User,
        };
        let cloned = data.clone();
        assert_eq!(data.content, cloned.content);
        assert_eq!(data.mode, cloned.mode);
        assert_eq!(data.source, cloned.source);
    }

    #[test]
    fn from_input_user_data() {
        let input = crate::message::event_input::UserData {
            base: EventBase::default(),
            content: "你好".into(),
            mode: UserMessageMode::Pending,
            source: UserMessageSource::User,
        };
        let output: UserMessageData = input.into();
        assert_eq!(output.content, "你好");
        assert_eq!(output.mode, UserMessageMode::Pending);
        assert_eq!(output.source, UserMessageSource::User);
    }
}
