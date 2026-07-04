//! 中断输出事件
//!
//! 引擎中断轮次后发出，CLI 据此渲染中断信息。
//! 所有中断通知统一走 OutputEvent::Interrupt，便于后续统计。
//!
//! 字段与 event_input::InterruptData 保持一致，
//! 转换时通过 From<InputInterruptData> 确保字段同步，避免手动构造导致不一致。

use crate::message::EventBase;
use crate::message::event_input::InterruptSource;

/// 中断输出数据
///
/// 与 event_input::InterruptData 字段一致，但独立定义以便后续拓展输出特有字段。
/// 通过 `From<event_input::InterruptData>` 确保输入→输出转换时字段同步。
#[derive(Debug, Clone, serde::Serialize)]
pub struct InterruptData {
    /// 事件基类（时间戳等公共字段）
    pub base: EventBase,
    /// 中断原因
    pub reason: String,
    /// 中断来源
    pub source: InterruptSource,
}

/// 输入中断事件 → 输出中断事件
///
/// 确保转换时字段完全一致，避免手动构造导致字段不同步。
/// 后续如果输出需要拓展字段，在此处添加默认值即可。
impl From<crate::message::event_input::InterruptData> for InterruptData {
    fn from(input: crate::message::event_input::InterruptData) -> Self {
        Self {
            base: input.base,
            reason: input.reason,
            source: input.source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    #[test]
    fn interrupt_data_holds_fields() {
        let data = InterruptData {
            base: EventBase::default(),
            reason: "循环检测".into(),
            source: InterruptSource::Hook,
        };
        assert_eq!(data.reason, "循环检测");
        assert_eq!(data.source, InterruptSource::Hook);
    }

    #[test]
    fn interrupt_data_user_source() {
        let data = InterruptData {
            base: EventBase::default(),
            reason: "用户取消".into(),
            source: InterruptSource::User,
        };
        assert_eq!(data.source, InterruptSource::User);
    }

    #[test]
    fn interrupt_data_clone_works() {
        let data = InterruptData {
            base: EventBase::default(),
            reason: "clone测试".into(),
            source: InterruptSource::System,
        };
        let cloned = data.clone();
        assert_eq!(data.reason, cloned.reason);
        assert_eq!(data.source, cloned.source);
    }

    #[test]
    fn from_input_interrupt_data() {
        let input = crate::message::event_input::InterruptData {
            base: EventBase::default(),
            reason: "循环检测".into(),
            source: InterruptSource::Hook,
        };
        let output: InterruptData = input.into();
        assert_eq!(output.reason, "循环检测");
        assert_eq!(output.source, InterruptSource::Hook);
    }
}
