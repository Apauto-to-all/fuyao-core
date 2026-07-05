//! 流式输出块事件
//!
//! LLM 流式输出时，每个文本/推理片段作为一个 Chunk 事件推送。
//! 前端收到后追加到当前消息的对应字段中。

use crate::message::EventBase;

/// 流式输出块事件 envelope
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChunkMessage {
    /// 事件元信息（id/timestamp）
    pub base: EventBase,
    /// 流式输出块载荷
    pub payload: ChunkPayload,
}

/// 流式输出块载荷
///
/// 包含 LLM 返回的文本内容或推理内容的增量片段。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChunkPayload {
    /// 文本内容片段
    pub content: Option<String>,
    /// 推理内容片段
    pub reasoning: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    #[test]
    fn chunk_with_content_only() {
        let msg = ChunkMessage {
            base: EventBase::default(),
            payload: ChunkPayload {
                content: Some("text".into()),
                reasoning: None,
            },
        };
        assert_eq!(msg.payload.content.as_deref(), Some("text"));
        assert!(msg.payload.reasoning.is_none());
    }

    #[test]
    fn chunk_with_reasoning_only() {
        let msg = ChunkMessage {
            base: EventBase::default(),
            payload: ChunkPayload {
                content: None,
                reasoning: Some("thinking".into()),
            },
        };
        assert!(msg.payload.content.is_none());
        assert_eq!(msg.payload.reasoning.as_deref(), Some("thinking"));
    }

    #[test]
    fn chunk_with_both_fields() {
        let msg = ChunkMessage {
            base: EventBase::default(),
            payload: ChunkPayload {
                content: Some("text".into()),
                reasoning: Some("thinking".into()),
            },
        };
        assert_eq!(msg.payload.content.as_deref(), Some("text"));
        assert_eq!(msg.payload.reasoning.as_deref(), Some("thinking"));
    }

    #[test]
    fn chunk_both_none() {
        let msg = ChunkMessage {
            base: EventBase::default(),
            payload: ChunkPayload {
                content: None,
                reasoning: None,
            },
        };
        assert!(msg.payload.content.is_none());
        assert!(msg.payload.reasoning.is_none());
    }

    #[test]
    fn chunk_clone_works() {
        let msg = ChunkMessage {
            base: EventBase::default(),
            payload: ChunkPayload {
                content: Some("clone".into()),
                reasoning: None,
            },
        };
        let cloned = msg.clone();
        assert_eq!(msg.payload.content, cloned.payload.content);
        assert_eq!(msg.payload.reasoning, cloned.payload.reasoning);
    }

    #[test]
    fn chunk_timestamp_is_set() {
        let msg = ChunkMessage {
            base: EventBase::default(),
            payload: ChunkPayload {
                content: None,
                reasoning: None,
            },
        };
        assert!(msg.base.timestamp > 0.0);
    }
}
