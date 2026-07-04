//! 中断事件
//!
//! 定义中断相关的数据结构。
//! 所有中断路径（用户、钩子、系统）统一走 InputEvent::Interrupt，
//! 便于后续统计和维护。
//!
//! 中断是纯粹的停止信号，不耦合注入消息等附加行为。
//! 如需在中断后注入引导消息，应额外发送 InputEvent::User 事件。

use crate::message::EventBase;

/// 中断来源
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum InterruptSource {
    /// 用户主动中断（Ctrl+C / ESC 等）
    User,
    /// 钩子拦截中断（循环检测等）
    Hook,
    /// 系统中断（超时、错误等自动触发）
    System,
}

/// 中断事件数据
///
/// 当中断触发时，由 InputEvent::Interrupt 携带。
/// Engine 收到后应停止当前正在执行的任务。
#[derive(Debug, Clone)]
pub struct InterruptData {
    /// 事件基类（时间戳等公共字段）
    pub base: EventBase,
    /// 中断原因描述（例如："用户取消"、"循环检测"）
    pub reason: String,
    /// 中断来源
    pub source: InterruptSource,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    #[test]
    fn interrupt_data_holds_reason() {
        let data = InterruptData {
            base: EventBase::default(),
            reason: "超时自动中断".into(),
            source: InterruptSource::System,
        };
        assert_eq!(data.reason, "超时自动中断");
        assert_eq!(data.source, InterruptSource::System);
    }

    #[test]
    fn interrupt_data_user_interrupt() {
        let data = InterruptData {
            base: EventBase::default(),
            reason: "用户取消".into(),
            source: InterruptSource::User,
        };
        assert_eq!(data.source, InterruptSource::User);
    }

    #[test]
    fn interrupt_data_hook_interrupt() {
        let data = InterruptData {
            base: EventBase::default(),
            reason: "循环检测".into(),
            source: InterruptSource::Hook,
        };
        assert_eq!(data.source, InterruptSource::Hook);
    }

    #[test]
    fn interrupt_data_clone_works() {
        let data = InterruptData {
            base: EventBase::default(),
            reason: "用户取消".into(),
            source: InterruptSource::User,
        };
        let cloned = data.clone();
        assert_eq!(data.reason, cloned.reason);
        assert_eq!(data.source, cloned.source);
    }

    #[test]
    fn interrupt_data_timestamp_is_set() {
        let data = InterruptData {
            base: EventBase::default(),
            reason: "测试".into(),
            source: InterruptSource::System,
        };
        assert!(data.base.timestamp > 0.0);
    }
}
