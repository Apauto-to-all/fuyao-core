//! 中断事件
//!
//! 定义中断相关的数据结构。
//! 所有中断路径（用户、钩子、引擎关闭）统一走 InputEvent::Interrupt，
//! 便于后续统计和维护。
//!
//! 中断是纯粹的停止信号，不耦合注入消息等附加行为。
//! 如需在中断后注入引导消息，应额外发送 InputEvent::User 事件。

use crate::message::EventBase;

/// 中断来源
///
/// 输入输出两侧共享（纯枚举，无方向语义）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum InterruptSource {
    /// 用户主动中断（Ctrl+C / ESC 等）
    User,
    /// 钩子拦截中断（循环检测等）
    Hook,
    /// 引擎关闭时由 shutdown 流程触发（让活跃 session 立即落库退出）
    Shutdown,
    /// 引擎停止原语（stop_session）触发：应用编排层为管理操作（回退 / 删除等）清场，
    /// 打断在跑 turn 并等待其完全终止
    Stop,
}

/// 中断事件 envelope
///
/// 当中断触发时，由 InputEvent::Interrupt 携带。
/// Engine 收到后应停止当前正在执行的任务。
///
/// 注：与 `output::InterruptMessage` 字段当前完全一致，但故意独立定义、不共享类型（见方案第八节）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InterruptMessage {
    /// 事件元信息（seq/timestamp）
    pub base: EventBase,
    /// 中断载荷
    pub payload: InterruptPayload,
}

/// 中断载荷
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InterruptPayload {
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
    fn interrupt_payload_holds_reason() {
        let payload = InterruptPayload {
            reason: "超时自动中断".into(),
            source: InterruptSource::Shutdown,
        };
        assert_eq!(payload.reason, "超时自动中断");
        assert_eq!(payload.source, InterruptSource::Shutdown);
    }

    #[test]
    fn interrupt_payload_user_interrupt() {
        let payload = InterruptPayload {
            reason: "用户取消".into(),
            source: InterruptSource::User,
        };
        assert_eq!(payload.source, InterruptSource::User);
    }

    #[test]
    fn interrupt_payload_hook_interrupt() {
        let payload = InterruptPayload {
            reason: "循环检测".into(),
            source: InterruptSource::Hook,
        };
        assert_eq!(payload.source, InterruptSource::Hook);
    }

    #[test]
    fn interrupt_message_envelope_holds_base_and_payload() {
        let msg = InterruptMessage {
            base: EventBase::default(),
            payload: InterruptPayload {
                reason: "测试".into(),
                source: InterruptSource::Shutdown,
            },
        };
        assert!(msg.base.seq.is_none());
        assert_eq!(msg.payload.reason, "测试");
    }

    #[test]
    fn interrupt_payload_clone_works() {
        let payload = InterruptPayload {
            reason: "用户取消".into(),
            source: InterruptSource::User,
        };
        let cloned = payload.clone();
        assert_eq!(payload.reason, cloned.reason);
        assert_eq!(payload.source, cloned.source);
    }

    #[test]
    fn interrupt_message_timestamp_is_set() {
        let msg = InterruptMessage {
            base: EventBase::default(),
            payload: InterruptPayload {
                reason: "测试".into(),
                source: InterruptSource::Shutdown,
            },
        };
        assert!(msg.base.timestamp > 0.0);
    }
}
