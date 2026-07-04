//! 队列快照
//!
//! 用于 UI 端只读访问双队列的当前状态。
//! 通过 `EngineHandle::queue_snapshot()` 获取，不暴露内部锁结构。

use crate::message::UserMessageMode;

/// 队列快照条目（一条用户消息的只读视图）
#[derive(Debug, Clone)]
pub struct QueueSnapshotItem {
    /// 消息 ID（与 InputEvent::User.base.id 对应）
    pub id: String,
    /// 内容前 N 个字符的预览（N 由引擎决定，通常 30）
    pub content_preview: String,
    /// 消息模式（Guide / Pending）
    pub mode: UserMessageMode,
}

/// 队列快照
#[derive(Debug, Clone, Default)]
pub struct QueueSnapshot {
    /// 引导队列内容（按入队顺序，待消费）
    pub guide: Vec<QueueSnapshotItem>,
    /// 排队队列内容（按入队顺序，等待转移）
    pub pending: Vec<QueueSnapshotItem>,
}

impl QueueSnapshot {
    /// 总条目数
    pub fn total(&self) -> usize {
        self.guide.len() + self.pending.len()
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.guide.is_empty() && self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_default_is_empty() {
        let snap = QueueSnapshot::default();
        assert!(snap.is_empty());
        assert_eq!(snap.total(), 0);
    }

    #[test]
    fn snapshot_total_sums_both_queues() {
        let snap = QueueSnapshot {
            guide: vec![QueueSnapshotItem {
                id: "g1".into(),
                content_preview: "guide msg".into(),
                mode: UserMessageMode::Guide,
            }],
            pending: vec![
                QueueSnapshotItem {
                    id: "p1".into(),
                    content_preview: "pending 1".into(),
                    mode: UserMessageMode::Pending,
                },
                QueueSnapshotItem {
                    id: "p2".into(),
                    content_preview: "pending 2".into(),
                    mode: UserMessageMode::Pending,
                },
            ],
        };
        assert_eq!(snap.total(), 3);
        assert!(!snap.is_empty());
    }
}
