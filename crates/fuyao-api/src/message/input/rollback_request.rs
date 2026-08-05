//! 对话回退请求输入事件
//!
//! 定义用户 / 上层应用请求对话回退的输入消息。该消息经引擎入口（[`InputEvent::Rollback`]）
//! 送入控制通道，在 turn 边界触发回退执行——删目标 seq 之后的所有消息并重算会话状态。
//!
//! 与手动压缩（[`CompressRequest`]）同属控制类输入，经引擎入口转成 [`crate::ControlCommand`]
//! 投递控制通道，由 task 在 turn 边界自执行。回退结果不经请求-响应通道回传，而是通过
//! [`crate::message::OutputEvent::Rollback`] 事件经 per-session 出口通道流出（与压缩的
//! Started/Ended 三阶段事件同机制）。

use crate::message::EventBase;

/// 对话回退请求消息 envelope
///
/// 由 [`InputEvent::Rollback`] 携带。业务字段在 [`RollbackPayload`]（回退目标 seq）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RollbackRequest {
    /// 事件元信息（id/timestamp）
    pub base: EventBase,
    /// 回退载荷
    pub payload: RollbackPayload,
}

/// 对话回退请求载荷
///
/// 只有一个 `target_seq`——回退到的目标消息 seq。目标必须是用户消息或压缩消息
/// （中间态 assistant / tool 不可作目标，校验在 store 层回退执行体
/// `SessionStore::rollback_to` 内原子完成）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RollbackPayload {
    /// 回退到的目标消息 seq（目标本身保留，删 seq > target 的所有消息）
    pub target_seq: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    #[test]
    fn payload_holds_target_seq() {
        let payload = RollbackPayload { target_seq: 7 };
        assert_eq!(payload.target_seq, 7);
    }

    #[test]
    fn request_envelope_holds_base_and_payload() {
        let req = RollbackRequest {
            base: EventBase::default(),
            payload: RollbackPayload { target_seq: 9 },
        };
        assert!(!req.base.id.is_empty());
        assert_eq!(req.payload.target_seq, 9);
    }

    #[test]
    fn request_clone_works() {
        let req = RollbackRequest {
            base: EventBase::default(),
            payload: RollbackPayload { target_seq: 3 },
        };
        let cloned = req.clone();
        assert_eq!(req.payload.target_seq, cloned.payload.target_seq);
        assert_eq!(req.base.id, cloned.base.id);
    }

    #[test]
    fn request_serde_roundtrip() {
        let req = RollbackRequest {
            base: EventBase::default(),
            payload: RollbackPayload { target_seq: 15 },
        };
        let json = serde_json::to_string(&req).expect("序列化失败");
        let de: RollbackRequest = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(de.base.id, req.base.id);
        assert_eq!(de.payload.target_seq, 15);
    }
}
