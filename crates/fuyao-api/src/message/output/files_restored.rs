//! 文件回退结果事件
//!
//! 会话回退联动恢复文件后发出：载荷是恢复 / 删除两组文件清单，前端据此向用户
//! 展示文件变化。纯实时事件（不落库、不进对话历史）——回退删消息在先，事件只是
//! 文件侧结果的对外通知，属于「新生命周期信号首选加事件变体」约定的一次运用。
//!
//! 与 title 事件一样采用 envelope + payload 两层结构，对齐项目既有模式。

use crate::message::EventBase;

/// 文件回退结果事件 envelope
///
/// 携带 `base`（事件元信息 + session_id 全程标签）和 `payload`（恢复 / 删除清单）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FilesRestoredMessage {
    /// 事件元信息（timestamp / session_id；纯实时事件 seq 恒 None）
    pub base: EventBase,
    /// 回退结果载荷
    pub payload: FilesRestoredPayload,
}

/// 文件回退结果载荷（双清单）
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FilesRestoredPayload {
    /// 已恢复为回退点内容的文件清单（工作区相对路径，已排序去重）
    pub restored: Vec<String>,
    /// 已删除的文件清单（回退点之后新建的文件，已排序去重）
    pub deleted: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_carries_both_lists() {
        let payload = FilesRestoredPayload {
            restored: vec!["src/main.rs".into()],
            deleted: vec!["新 目录/新建 文件.txt".into()],
        };
        assert_eq!(payload.restored, vec!["src/main.rs"]);
        assert_eq!(payload.deleted.len(), 1);
    }

    #[test]
    fn message_envelope_constructs() {
        let msg = FilesRestoredMessage {
            base: EventBase::default(),
            payload: FilesRestoredPayload {
                restored: vec![],
                deleted: vec!["out.txt".into()],
            },
        };
        // 纯实时事件：seq 恒 None
        assert!(msg.base.seq.is_none());
        assert!(msg.base.timestamp > 0.0);
        assert_eq!(msg.payload.deleted, vec!["out.txt"]);
    }

    #[test]
    fn message_clone_works() {
        let msg = FilesRestoredMessage {
            base: EventBase::default(),
            payload: FilesRestoredPayload {
                restored: vec!["a.rs".into()],
                deleted: vec![],
            },
        };
        let cloned = msg.clone();
        assert_eq!(cloned.payload.restored, msg.payload.restored);
        assert_eq!(cloned.base.seq, msg.base.seq);
    }

    #[test]
    fn payload_serde_roundtrip() {
        let original = FilesRestoredMessage {
            base: EventBase::default(),
            payload: FilesRestoredPayload {
                restored: vec!["src/工具.rs".into()],
                deleted: vec!["tmp/out.txt".into()],
            },
        };
        let json = serde_json::to_string(&original).expect("序列化失败");
        let restored: FilesRestoredMessage = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(restored.payload.restored, vec!["src/工具.rs"]);
        assert_eq!(restored.payload.deleted, vec!["tmp/out.txt"]);
    }

    #[test]
    fn message_with_session_id_roundtrip() {
        // session_id 标签随事件流过管道后仍可恢复
        let mut msg = FilesRestoredMessage {
            base: EventBase::default(),
            payload: FilesRestoredPayload::default(),
        };
        msg.base.session_id = Some("sess-abc".into());

        let json = serde_json::to_string(&msg).expect("序列化失败");
        let restored: FilesRestoredMessage = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(restored.base.session_id.as_deref(), Some("sess-abc"));
    }

    /// FilesRestoredPayload 实现 Default（空清单起步，便于构造与测试）
    #[test]
    fn payload_default_is_empty_lists() {
        let payload = FilesRestoredPayload::default();
        assert!(payload.restored.is_empty());
        assert!(payload.deleted.is_empty());
    }
}
