//! 轮次开始事件
//!
//! 引擎在每次 ReAct 轮次开始时发出，供插件重置轮次级状态。

use crate::message::EventBase;

/// 轮次开始数据
#[derive(Debug, Clone, serde::Serialize)]
pub struct TurnStartData {
    /// 事件基类（时间戳等公共字段）
    pub base: EventBase,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_start_has_timestamp() {
        let data = TurnStartData {
            base: EventBase::default(),
        };
        assert!(data.base.timestamp > 0.0);
    }

    #[test]
    fn turn_start_clone_works() {
        let data = TurnStartData {
            base: EventBase::default(),
        };
        let cloned = data.clone();
        assert!(cloned.base.timestamp > 0.0);
    }
}
