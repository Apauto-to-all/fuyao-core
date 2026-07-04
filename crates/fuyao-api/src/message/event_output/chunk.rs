//! 流式输出块事件
//!
//! LLM 流式输出时，每个文本/推理片段作为一个 Chunk 事件推送。
//! 前端收到后追加到当前消息的对应字段中。

use crate::message::EventBase;

/// 流式输出块数据
///
/// 包含 LLM 返回的文本内容或推理内容的增量片段。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChunkData {
    /// 事件基类（时间戳等公共字段）
    pub base: EventBase,
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
        let chunk = ChunkData {
            base: EventBase::default(),
            content: Some("text".into()),
            reasoning: None,
        };
        assert_eq!(chunk.content.as_deref(), Some("text"));
        assert!(chunk.reasoning.is_none());
    }

    #[test]
    fn chunk_with_reasoning_only() {
        let chunk = ChunkData {
            base: EventBase::default(),
            content: None,
            reasoning: Some("thinking".into()),
        };
        assert!(chunk.content.is_none());
        assert_eq!(chunk.reasoning.as_deref(), Some("thinking"));
    }

    #[test]
    fn chunk_with_both_fields() {
        let chunk = ChunkData {
            base: EventBase::default(),
            content: Some("text".into()),
            reasoning: Some("thinking".into()),
        };
        assert_eq!(chunk.content.as_deref(), Some("text"));
        assert_eq!(chunk.reasoning.as_deref(), Some("thinking"));
    }

    #[test]
    fn chunk_both_none() {
        let chunk = ChunkData {
            base: EventBase::default(),
            content: None,
            reasoning: None,
        };
        assert!(chunk.content.is_none());
        assert!(chunk.reasoning.is_none());
    }

    #[test]
    fn chunk_clone_works() {
        let chunk = ChunkData {
            base: EventBase::default(),
            content: Some("clone".into()),
            reasoning: None,
        };
        let cloned = chunk.clone();
        assert_eq!(chunk.content, cloned.content);
        assert_eq!(chunk.reasoning, cloned.reasoning);
    }

    #[test]
    fn chunk_timestamp_is_set() {
        let chunk = ChunkData {
            base: EventBase::default(),
            content: None,
            reasoning: None,
        };
        assert!(chunk.base.timestamp > 0.0);
    }
}
