//! 会话标题事件
//!
//! 首轮对话后异步生成标题，标题落库后发出本事件，前端据此更新会话列表标题。
//! 事件经统一消息处理管道（dispatch）发送：可被 output_intercept 钩子拦截改写，
//! 也会触发 output_observe 钩子（如审计日志）。
//!
//! 与 compression 事件一样采用 envelope + payload 两层结构，对齐项目既有模式。

use crate::message::EventBase;

/// 会话标题事件 envelope
///
/// 携带 `base`（事件元信息 + session_id 全程标签）和 `payload`（新标题文本）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TitleMessage {
    /// 事件元信息（seq/timestamp/session_id）
    pub base: EventBase,
    /// 标题载荷
    pub payload: TitlePayload,
}

/// 标题载荷（扁平结构，无阶段区分）
///
/// 与 CompressionPayload 的三阶段 tagging 不同——标题是一次性产出，无需多阶段。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TitlePayload {
    /// 新标题文本
    pub title: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_carries_title() {
        let payload = TitlePayload {
            title: "Rust 异步讨论".into(),
        };
        assert_eq!(payload.title, "Rust 异步讨论");
    }

    #[test]
    fn message_envelope_constructs() {
        let msg = TitleMessage {
            base: EventBase::default(),
            payload: TitlePayload {
                title: "测试标题".into(),
            },
        };
        assert!(msg.base.timestamp > 0.0);
        assert_eq!(msg.payload.title, "测试标题");
    }

    #[test]
    fn message_clone_works() {
        let msg = TitleMessage {
            base: EventBase::default(),
            payload: TitlePayload {
                title: "原始".into(),
            },
        };
        let cloned = msg.clone();
        assert_eq!(cloned.payload.title, "原始");
        assert_eq!(cloned.base.seq, msg.base.seq);
    }

    #[test]
    fn payload_serde_roundtrip() {
        let original = TitleMessage {
            base: EventBase::default(),
            payload: TitlePayload {
                title: "序列化测试".into(),
            },
        };
        let json = serde_json::to_string(&original).expect("序列化失败");
        let restored: TitleMessage = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(restored.payload.title, "序列化测试");
    }

    #[test]
    fn message_with_session_id_roundtrip() {
        // 验证 session_id 标签随事件流过管道后仍可恢复
        let mut msg = TitleMessage {
            base: EventBase::default(),
            payload: TitlePayload {
                title: "带 session".into(),
            },
        };
        msg.base.session_id = Some("sess-abc".into());

        let json = serde_json::to_string(&msg).expect("序列化失败");
        assert!(
            json.contains(r#""session_id":"sess-abc""#),
            "session_id 应出现在 JSON 中: {json}"
        );

        let restored: TitleMessage = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(restored.base.session_id.as_deref(), Some("sess-abc"));
    }
}
