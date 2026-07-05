//! 轮次开始事件
//!
//! 引擎在每次 ReAct 轮次开始时发出，供插件重置轮次级状态。

use crate::message::EventBase;

/// 轮次开始事件 envelope（无 payload，仅 base）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TurnStartMessage {
    /// 事件元信息（id/timestamp）
    pub base: EventBase,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_start_has_timestamp() {
        let msg = TurnStartMessage {
            base: EventBase::default(),
        };
        assert!(msg.base.timestamp > 0.0);
    }

    #[test]
    fn turn_start_clone_works() {
        let msg = TurnStartMessage {
            base: EventBase::default(),
        };
        let cloned = msg.clone();
        assert!(cloned.base.timestamp > 0.0);
    }
}
