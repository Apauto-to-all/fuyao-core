//! 错误事件

use crate::message::EventBase;

/// 错误事件 envelope
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ErrorMessage {
    /// 事件元信息（id/timestamp）
    pub base: EventBase,
    /// 错误载荷
    pub payload: ErrorPayload,
}

/// 错误载荷
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ErrorPayload {
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
        let msg = ErrorMessage {
            base: EventBase::default(),
            payload: ErrorPayload {
                message: "出错了".into(),
                recoverable: false,
            },
        };
        assert_eq!(msg.payload.message, "出错了");
        assert!(!msg.payload.recoverable);
    }

    #[test]
    fn error_can_be_recoverable() {
        let msg = ErrorMessage {
            base: EventBase::default(),
            payload: ErrorPayload {
                message: "可恢复错误".into(),
                recoverable: true,
            },
        };
        assert!(msg.payload.recoverable);
    }

    #[test]
    fn error_clone_works() {
        let msg = ErrorMessage {
            base: EventBase::default(),
            payload: ErrorPayload {
                message: "clone".into(),
                recoverable: true,
            },
        };
        let cloned = msg.clone();
        assert_eq!(msg.payload.message, cloned.payload.message);
        assert_eq!(msg.payload.recoverable, cloned.payload.recoverable);
    }

    #[test]
    fn error_timestamp_is_set() {
        let msg = ErrorMessage {
            base: EventBase::default(),
            payload: ErrorPayload {
                message: "时间测试".into(),
                recoverable: false,
            },
        };
        assert!(msg.base.timestamp > 0.0);
    }
}
