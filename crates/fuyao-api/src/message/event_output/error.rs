//! 错误事件

use crate::message::EventBase;

/// 错误数据
#[derive(Debug, Clone, serde::Serialize)]
pub struct ErrorData {
    /// 事件基类（时间戳等公共字段）
    pub base: EventBase,
    /// 错误消息
    pub message: String,
    /// 是否可恢复
    pub recoverable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    #[test]
    fn error_holds_message_and_not_recoverable() {
        let err = ErrorData {
            base: EventBase::default(),
            message: "出错了".into(),
            recoverable: false,
        };
        assert_eq!(err.message, "出错了");
        assert!(!err.recoverable);
    }

    #[test]
    fn error_can_be_recoverable() {
        let err = ErrorData {
            base: EventBase::default(),
            message: "可恢复错误".into(),
            recoverable: true,
        };
        assert!(err.recoverable);
    }

    #[test]
    fn error_clone_works() {
        let err = ErrorData {
            base: EventBase::default(),
            message: "clone".into(),
            recoverable: true,
        };
        let cloned = err.clone();
        assert_eq!(err.message, cloned.message);
        assert_eq!(err.recoverable, cloned.recoverable);
    }

    #[test]
    fn error_timestamp_is_set() {
        let err = ErrorData {
            base: EventBase::default(),
            message: "时间测试".into(),
            recoverable: false,
        };
        assert!(err.base.timestamp > 0.0);
    }
}
