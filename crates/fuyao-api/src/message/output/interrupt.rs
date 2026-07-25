//! 中断输出事件
//!
//! 引擎中断轮次后发出，CLI 据此渲染中断信息。
//! 所有中断通知统一走 OutputEvent::Interrupt，便于后续统计。
//!
//! 注：与 `input::InterruptMessage` 字段当前完全一致，但故意独立定义、不共享类型（见方案第八节）。
//! InterruptSource 为纯枚举（无方向语义），输入输出共享同一个类型（定义在 input 侧）。

use crate::message::EventBase;
use crate::message::input::InterruptSource;

/// 中断输出事件 envelope
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InterruptMessage {
    /// 事件元信息（id/timestamp）
    pub base: EventBase,
    /// 中断载荷
    pub payload: InterruptPayload,
}

/// 中断输出载荷
///
/// 与 input::InterruptPayload 字段一致，但独立定义以便后续拓展输出特有字段。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InterruptPayload {
    /// 中断原因
    pub reason: String,
    /// 中断来源
    pub source: InterruptSource,
}

impl InterruptPayload {
    /// 构造中断载荷
    ///
    /// 内核链路只认 output 侧类型，入口（Engine::send 转化、SessionSender 直产）
    /// 统一用此方法构造，避免散落的字段照搬样板。
    pub fn new(reason: impl Into<String>, source: InterruptSource) -> Self {
        Self {
            reason: reason.into(),
            source,
        }
    }
}

impl InterruptMessage {
    /// 构造中断事件（base 取默认值，自动生成 id/timestamp）
    pub fn new(reason: impl Into<String>, source: InterruptSource) -> Self {
        Self {
            base: EventBase::default(),
            payload: InterruptPayload::new(reason, source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    #[test]
    fn interrupt_payload_holds_fields() {
        let msg = InterruptMessage {
            base: EventBase::default(),
            payload: InterruptPayload {
                reason: "循环检测".into(),
                source: InterruptSource::Hook,
            },
        };
        assert_eq!(msg.payload.reason, "循环检测");
        assert_eq!(msg.payload.source, InterruptSource::Hook);
    }

    #[test]
    fn interrupt_payload_user_source() {
        let msg = InterruptMessage {
            base: EventBase::default(),
            payload: InterruptPayload {
                reason: "用户取消".into(),
                source: InterruptSource::User,
            },
        };
        assert_eq!(msg.payload.source, InterruptSource::User);
    }

    #[test]
    fn interrupt_payload_clone_works() {
        let msg = InterruptMessage {
            base: EventBase::default(),
            payload: InterruptPayload {
                reason: "clone测试".into(),
                source: InterruptSource::Shutdown,
            },
        };
        let cloned = msg.clone();
        assert_eq!(msg.payload.reason, cloned.payload.reason);
        assert_eq!(msg.payload.source, cloned.payload.source);
    }
}
