//! 队列更新事件
//!
//! 双队列机制的状态通知：当 guide / pending 队列发生变化时，
//! 引擎主动 emit 此事件，UI 据此呈现队列深度与排队进度。
//!
//! 设计要点：
//! - 携带当前队列长度快照（非增量），UI 即使丢失中间事件也能恢复正确状态
//! - envelope.base.id 在「入队 → 消费」链路上复用用户消息的 base.id，
//!   便于 UI 精确配对临时气泡与正式气泡
//! - drain_pending_to_guide 是批量操作，envelope.base.id 用默认值（无单一关联）

use crate::message::EventBase;

/// 队列更新事件 envelope
///
/// base.id 复用语义：
/// - Enqueued / Consumed 时复用对应 UserMessage 的 base.id
/// - Transferred 时使用默认值（批量转移无单一关联）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QueueUpdateMessage {
    /// 事件元信息（id/timestamp）
    ///
    /// Enqueued / Consumed 时复用对应 UserMessage 的 base.id；
    /// Transferred 时使用默认值（批量转移无单一关联）。
    pub base: EventBase,
    /// 队列更新载荷
    pub payload: QueueUpdatePayload,
}

/// 队列更新载荷
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QueueUpdatePayload {
    /// 引导队列当前长度（含已入队未消费）
    pub guide_count: usize,
    /// 排队队列当前长度
    pub pending_count: usize,
    /// 变化种类
    pub kind: QueueUpdateKind,
}

/// 队列变化种类
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum QueueUpdateKind {
    /// 用户消息入队（guide 或 pending）
    Enqueued,
    /// TurnExecutor 消费一条 guide 消息
    Consumed,
    /// pending 全部转移到 guide
    Transferred,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_update_holds_fields() {
        let msg = QueueUpdateMessage {
            base: EventBase::default(),
            payload: QueueUpdatePayload {
                guide_count: 2,
                pending_count: 1,
                kind: QueueUpdateKind::Enqueued,
            },
        };
        assert_eq!(msg.payload.guide_count, 2);
        assert_eq!(msg.payload.pending_count, 1);
        assert_eq!(msg.payload.kind, QueueUpdateKind::Enqueued);
    }

    #[test]
    fn queue_update_clone_works() {
        let msg = QueueUpdateMessage {
            base: EventBase::default(),
            payload: QueueUpdatePayload {
                guide_count: 1,
                pending_count: 0,
                kind: QueueUpdateKind::Consumed,
            },
        };
        let cloned = msg.clone();
        assert_eq!(msg.payload.guide_count, cloned.payload.guide_count);
        assert_eq!(msg.payload.pending_count, cloned.payload.pending_count);
        assert_eq!(msg.payload.kind, cloned.payload.kind);
    }

    #[test]
    fn queue_update_kind_equality() {
        assert_eq!(QueueUpdateKind::Enqueued, QueueUpdateKind::Enqueued);
        assert_ne!(QueueUpdateKind::Enqueued, QueueUpdateKind::Consumed);
        assert_ne!(QueueUpdateKind::Consumed, QueueUpdateKind::Transferred);
    }
}
