//! 用户输入事件
//!
//! 定义用户消息相关的事件类型和数据结构。
//! - `UserMessageMode`: 消息处理模式（引导/排队）
//! - `UserMessageSource`: 消息来源（用户输入/系统注入/插件注入）
//! - `SystemSource`: 系统注入来源详情
//! - `PluginSource`: 插件注入来源详情
//! - `UserData`: 消息内容 + 模式 + 来源

/// 用户消息模式
///
/// 控制消息的入队和投递时机：
/// - `Guide`: 进入引导队列，AI 完成一轮对话（如工具调用完成）后立即投递
/// - `Pending`: 进入排队队列，等待 AI 不再调用工具后才投递
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum UserMessageMode {
    /// 引导模式：AI 完成一轮对话后立即投递
    Guide,
    /// 排队模式：AI 不再调用工具后才投递
    Pending,
}

/// 默认使用引导模式
impl Default for UserMessageMode {
    fn default() -> Self {
        Self::Guide
    }
}

/// 系统注入来源详情
///
/// 携带系统注入的原因标识，便于追踪和统计。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SystemSource {
    /// 系统注入原因标识（如 "timeout"、"error_recovery"）
    pub reason: String,
}

/// 插件注入来源详情
///
/// 携带插件名称，便于追踪是哪个插件注入的消息。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PluginSource {
    /// 插件名称（如 "loop_guard"）
    pub name: String,
}

/// 用户消息来源
///
/// 区分消息的来源：
/// - `User`: 用户在界面中主动输入
/// - `System`: 系统注入（如超时提醒、错误恢复等），携带 SystemSource 详情
/// - `Plugin`: 插件注入（如循环检测后注入引导消息），携带 PluginSource 详情
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum UserMessageSource {
    /// 用户主动输入
    User,
    /// 系统注入（携带注入原因）
    System(SystemSource),
    /// 插件注入（携带插件名称）
    Plugin(PluginSource),
}

/// 默认来源为用户输入
impl Default for UserMessageSource {
    fn default() -> Self {
        Self::User
    }
}

use crate::message::EventBase;

/// 用户消息事件数据
///
/// 当用户在界面中发送消息时，由 InputEvent::User 携带。
/// 也可用于系统或插件注入引导消息（source = System/Plugin）。
#[derive(Debug, Clone)]
pub struct UserData {
    /// 事件基类（时间戳等公共字段）
    pub base: EventBase,
    /// 消息文本内容（用户输入的文字）
    pub content: String,
    /// 消息模式：guide（引导）或 pending（排队），默认 guide
    pub mode: UserMessageMode,
    /// 消息来源：用户输入、系统注入或插件注入，默认 user
    pub source: UserMessageSource,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    /// 默认模式应为 Guide
    #[test]
    fn default_mode_is_guide() {
        assert_eq!(UserMessageMode::default(), UserMessageMode::Guide);
    }

    /// 默认来源应为 User
    #[test]
    fn default_source_is_user() {
        assert_eq!(UserMessageSource::default(), UserMessageSource::User);
    }

    #[test]
    fn user_data_holds_fields() {
        let data = UserData {
            base: EventBase::default(),
            content: "你好".into(),
            mode: UserMessageMode::Pending,
            source: UserMessageSource::User,
        };
        assert_eq!(data.content, "你好");
        assert_eq!(data.mode, UserMessageMode::Pending);
        assert_eq!(data.source, UserMessageSource::User);
    }

    #[test]
    fn user_data_inject_source() {
        let data = UserData {
            base: EventBase::default(),
            content: "检测到循环，请调整策略".into(),
            mode: UserMessageMode::Guide,
            source: UserMessageSource::Plugin(PluginSource {
                name: "loop_guard".into(),
            }),
        };
        assert_eq!(
            data.source,
            UserMessageSource::Plugin(PluginSource {
                name: "loop_guard".into(),
            })
        );
    }

    #[test]
    fn user_data_system_source() {
        let data = UserData {
            base: EventBase::default(),
            content: "超时提醒".into(),
            mode: UserMessageMode::Guide,
            source: UserMessageSource::System(SystemSource {
                reason: "timeout".into(),
            }),
        };
        assert_eq!(
            data.source,
            UserMessageSource::System(SystemSource {
                reason: "timeout".into(),
            })
        );
    }

    #[test]
    fn user_message_mode_equality() {
        assert_eq!(UserMessageMode::Guide, UserMessageMode::Guide);
        assert_eq!(UserMessageMode::Pending, UserMessageMode::Pending);
        assert_ne!(UserMessageMode::Guide, UserMessageMode::Pending);
    }

    #[test]
    fn user_data_clone_works() {
        let data = UserData {
            base: EventBase::default(),
            content: "clone测试".into(),
            mode: UserMessageMode::Guide,
            source: UserMessageSource::User,
        };
        let cloned = data.clone();
        assert_eq!(data.content, cloned.content);
        assert_eq!(data.mode, cloned.mode);
        assert_eq!(data.source, cloned.source);
    }

    #[test]
    fn user_data_timestamp_is_set() {
        let data = UserData {
            base: EventBase::default(),
            content: "时间测试".into(),
            mode: UserMessageMode::Guide,
            source: UserMessageSource::User,
        };
        assert!(data.base.timestamp > 0.0);
    }
}
