//! 上下文压缩事件
//!
//! 压缩发生时按阶段推送：Started（开始）→ Delta（摘要流式增量）→ Ended（完成）
//! 或 Failed（失败）。前端据此完整追踪压缩生命周期——显示"压缩中..."状态、
//! 实时渲染正在生成的摘要、压缩完成展示摘要、失败时解除"压缩中"状态并提示原因。
//!
//! 终态保证：Started 发出后必有 Ended / Failed 之一，前端不会挂起在"压缩中"状态。
//!
//! 与主对话流的 Chunk 事件语义不同：Chunk 是 AI 回复的增量，Compression Delta 是
//! 压缩 LLM 摘要的增量，前端渲染位置/样式不同。

use crate::message::EventBase;

/// 压缩触发原因（事件层独立定义，与 fuyao-session::CompressionReason 同构）
///
/// 复制而非复用：fuyao-api 不能反向依赖 fuyao-session（L0 ← L3 禁止）。
/// fuyao-core 在事件发布时做 `fuyao_session::CompressionReason → 本枚举` 的转换。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompressionReason {
    /// 阈值自动触发
    Auto,
    /// 用户手动触发
    Manual,
    /// Provider overflow 错误后被动触发
    Overflow,
}

/// 上下文压缩事件 envelope
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompressionMessage {
    /// 事件元信息（id/timestamp/session_id）
    pub base: EventBase,
    /// 压缩载荷（按 mode 区分三阶段）
    pub payload: CompressionPayload,
}

/// 压缩载荷（内部 tagging：`tag = "mode"`）
///
/// JSON 形如 `{"mode": "started", "reason": "auto", ...}`，前端按 mode 字段分流。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum CompressionPayload {
    /// 压缩开始（调摘要 LLM 之前发出，前端显示"压缩中..."状态）
    Started(CompressionStartedPayload),
    /// 摘要流式增量（每个 TextDelta/ReasoningDelta 推送一次，前端实时拼接摘要）
    Delta(CompressionDeltaPayload),
    /// 压缩完成（apply 落库成功后发出，前端移除"压缩中"状态、展示统计）
    Ended(CompressionEndedPayload),
    /// 压缩失败（Started 之后任一步失败时发出，前端解除"压缩中"状态并提示原因）；
    /// live-only 事件——失败的压缩不落库，历史回放中不出现
    Failed(CompressionFailedPayload),
}

/// Started 阶段载荷
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompressionStartedPayload {
    /// 触发原因（auto / manual / overflow）
    pub reason: CompressionReason,
}

/// Delta 阶段载荷（字段对齐 ChunkPayload，区分思考和正文）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompressionDeltaPayload {
    /// 摘要正文片段（增量）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// 思考内容片段（增量）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

/// Ended 阶段载荷
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompressionEndedPayload {
    /// 触发原因（与 Started 一致）
    pub reason: CompressionReason,
    /// 完整摘要正文（content 全文）
    pub content: String,
    /// 压缩思考全文（ReasoningDelta 累积；随 compaction 边界消息落库于 reasoning 列，
    /// 实时 Ended 与历史回放同构携带；非推理模型压缩时缺省）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// 新 compaction 边界消息的 seq（前端定位压缩在对话流中的位置）
    pub new_seq: i64,
}

/// Failed 阶段载荷
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompressionFailedPayload {
    /// 触发原因（与 Started 一致）
    pub reason: CompressionReason,
    /// 失败原因（人类可读描述，用于前端展示；已脱敏的错误链文本）
    pub cause: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_payload_carries_trigger_info() {
        let payload = CompressionStartedPayload {
            reason: CompressionReason::Auto,
        };
        assert_eq!(payload.reason, CompressionReason::Auto);
    }

    #[test]
    fn delta_payload_supports_content_only() {
        let payload = CompressionDeltaPayload {
            content: Some("摘要片段".into()),
            reasoning: None,
        };
        assert_eq!(payload.content.as_deref(), Some("摘要片段"));
        assert!(payload.reasoning.is_none());
    }

    #[test]
    fn delta_payload_supports_reasoning_only() {
        let payload = CompressionDeltaPayload {
            content: None,
            reasoning: Some("思考片段".into()),
        };
        assert!(payload.content.is_none());
        assert_eq!(payload.reasoning.as_deref(), Some("思考片段"));
    }

    #[test]
    fn delta_payload_supports_both() {
        let payload = CompressionDeltaPayload {
            content: Some("正文".into()),
            reasoning: Some("思考".into()),
        };
        assert_eq!(payload.content.as_deref(), Some("正文"));
        assert_eq!(payload.reasoning.as_deref(), Some("思考"));
    }

    #[test]
    fn ended_payload_carries_full_result() {
        let payload = CompressionEndedPayload {
            reason: CompressionReason::Manual,
            content: "完整摘要".into(),
            reasoning: Some("压缩思考全文".into()),
            new_seq: 42,
        };
        assert_eq!(payload.reason, CompressionReason::Manual);
        assert_eq!(payload.content, "完整摘要");
        assert_eq!(payload.reasoning.as_deref(), Some("压缩思考全文"));
        assert_eq!(payload.new_seq, 42);
    }

    #[test]
    fn failed_payload_carries_cause() {
        let payload = CompressionFailedPayload {
            reason: CompressionReason::Manual,
            cause: "LLM 调用失败: 速率限制".into(),
        };
        assert_eq!(payload.reason, CompressionReason::Manual);
        assert_eq!(payload.cause, "LLM 调用失败: 速率限制");
    }

    #[test]
    fn message_started_envelope_constructs() {
        let msg = CompressionMessage {
            base: EventBase::default(),
            payload: CompressionPayload::Started(CompressionStartedPayload {
                reason: CompressionReason::Auto,
            }),
        };
        assert!(msg.base.timestamp > 0.0);
        assert!(matches!(msg.payload, CompressionPayload::Started(_)));
    }

    #[test]
    fn message_delta_envelope_constructs() {
        let msg = CompressionMessage {
            base: EventBase::default(),
            payload: CompressionPayload::Delta(CompressionDeltaPayload {
                content: Some("增量".into()),
                reasoning: None,
            }),
        };
        assert!(matches!(msg.payload, CompressionPayload::Delta(_)));
    }

    #[test]
    fn message_ended_envelope_constructs() {
        let msg = CompressionMessage {
            base: EventBase::default(),
            payload: CompressionPayload::Ended(CompressionEndedPayload {
                reason: CompressionReason::Auto,
                content: "完整".into(),
                reasoning: None,
                new_seq: 1,
            }),
        };
        assert!(matches!(msg.payload, CompressionPayload::Ended(_)));
    }

    #[test]
    fn message_failed_envelope_constructs() {
        let msg = CompressionMessage {
            base: EventBase::default(),
            payload: CompressionPayload::Failed(CompressionFailedPayload {
                reason: CompressionReason::Manual,
                cause: "LLM 调用失败: 速率限制".into(),
            }),
        };
        assert!(matches!(msg.payload, CompressionPayload::Failed(_)));
    }

    #[test]
    fn payload_serde_failed_roundtrip() {
        let original = CompressionMessage {
            base: EventBase::default(),
            payload: CompressionPayload::Failed(CompressionFailedPayload {
                reason: CompressionReason::Auto,
                cause: "摘要生成失败".into(),
            }),
        };
        let json = serde_json::to_string(&original).expect("序列化失败");
        assert!(
            json.contains(r#""mode":"failed""#),
            "JSON 应含 mode=failed 标签，实际: {json}"
        );
        let restored: CompressionMessage = serde_json::from_str(&json).expect("反序列化失败");
        match restored.payload {
            CompressionPayload::Failed(p) => {
                assert_eq!(p.reason, CompressionReason::Auto);
                assert_eq!(p.cause, "摘要生成失败");
            }
            other => panic!("应反序列化为 Failed 变体，实际: {other:?}"),
        }
    }

    #[test]
    fn payload_serde_started_roundtrip() {
        let original = CompressionMessage {
            base: EventBase::default(),
            payload: CompressionPayload::Started(CompressionStartedPayload {
                reason: CompressionReason::Auto,
            }),
        };
        let json = serde_json::to_string(&original).expect("序列化失败");
        let restored: CompressionMessage = serde_json::from_str(&json).expect("反序列化失败");
        assert!(matches!(restored.payload, CompressionPayload::Started(_)));
    }

    #[test]
    fn payload_serde_uses_internal_tag_mode() {
        // 验证 #[serde(tag = "mode")] 生效：JSON 里含 "mode" 字段
        let msg = CompressionMessage {
            base: EventBase::default(),
            payload: CompressionPayload::Delta(CompressionDeltaPayload {
                content: Some("x".into()),
                reasoning: None,
            }),
        };
        let json = serde_json::to_string(&msg).expect("序列化失败");
        assert!(
            json.contains(r#""mode":"delta""#),
            "JSON 应含 mode=delta 标签，实际: {json}"
        );
    }

    #[test]
    fn reason_serde_snake_case() {
        // 验证 #[serde(rename_all = "snake_case")]：Auto → "auto"
        let json = serde_json::to_string(&CompressionReason::Overflow).unwrap();
        assert_eq!(json, "\"overflow\"");
    }

    #[test]
    fn reason_clone_copy_works() {
        let r1 = CompressionReason::Auto;
        let r2 = r1; // Copy
        assert_eq!(r1, r2);
    }
}
