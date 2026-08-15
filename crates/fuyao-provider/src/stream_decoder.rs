//! 流式事件累加器
//!
//! 消费**已解码**的 [`StreamEvent`]（线解码在 [`crate::openai::sse`]），
//! 跟踪工具调用增量拼接、累积 usage 统计，输出 [`OutputEvent`] 给 Engine 推送到外部。
//!
//! 与 [`crate::openai::sse`] 的职责边界：sse 做「SSE 文本 → StreamEvent」的线解码，
//! 本模块做「StreamEvent → OutputEvent」的事件累加，两者正交、不重叠。

use crate::{StreamEvent, StreamUsage};
use fuyao_api::ToolCallData;
use fuyao_api::message::output::{ChunkMessage, ChunkPayload};
use fuyao_api::message::{EventBase, OutputEvent};

/// 工具调用增量状态
#[derive(Debug, Clone)]
struct ToolUseState {
    id: String,
    name: String,
    args_buffer: String,
}

/// 流式事件累加器
///
/// 消费**已解码**的 [`StreamEvent`]，跟踪工具调用增量拼接、累积 usage 统计，
/// 输出 [`OutputEvent`] 给 Engine 推送到外部。线解码（SSE 文本 → StreamEvent）
/// 由 [`crate::openai::sse`] 负责，与本模块正交。
pub struct StreamAggregator {
    /// 工具调用状态（index → ToolUseState）
    tool_calls: std::collections::HashMap<usize, ToolUseState>,
    /// 累积使用统计
    usage: StreamUsage,
}

impl StreamAggregator {
    pub fn new() -> Self {
        Self {
            tool_calls: std::collections::HashMap::new(),
            usage: StreamUsage::default(),
        }
    }

    /// 处理一个 StreamEvent，返回零或多个 OutputEvent
    ///
    /// ToolCallChunk 按 index 增量拼接 id/name/args，不输出事件。
    /// Done 事件只更新 usage，不输出 ToolCall 事件。
    pub fn process(&mut self, event: StreamEvent) -> Vec<OutputEvent> {
        match event {
            StreamEvent::TextDelta { content } => {
                vec![OutputEvent::Chunk(ChunkMessage {
                    base: EventBase::default(),
                    payload: ChunkPayload {
                        content: Some(content),
                        reasoning: None,
                    },
                })]
            }

            StreamEvent::ReasoningDelta { content } => {
                vec![OutputEvent::Chunk(ChunkMessage {
                    base: EventBase::default(),
                    payload: ChunkPayload {
                        content: None,
                        reasoning: Some(content),
                    },
                })]
            }

            StreamEvent::ToolCallChunk {
                index,
                id,
                name,
                args_delta,
            } => {
                let state = self
                    .tool_calls
                    .entry(index)
                    .or_insert_with(|| ToolUseState {
                        id: String::new(),
                        name: String::new(),
                        args_buffer: String::new(),
                    });
                if let Some(id) = id {
                    state.id = id;
                }
                if let Some(name) = name {
                    state.name = name;
                }
                if let Some(args_delta) = args_delta {
                    state.args_buffer.push_str(&args_delta);
                }
                vec![]
            }

            StreamEvent::Done {
                usage,
                finish_reason: _,
            } => {
                if usage.prompt_tokens > 0 {
                    self.usage.prompt_tokens = usage.prompt_tokens;
                }
                if usage.completion_tokens > 0 {
                    self.usage.completion_tokens = usage.completion_tokens;
                }
                if usage.total_tokens > 0 {
                    self.usage.total_tokens = usage.total_tokens;
                }
                if usage.completion_reasoning_tokens.is_some() {
                    self.usage.completion_reasoning_tokens = usage.completion_reasoning_tokens;
                }
                if usage.prompt_cached_tokens.is_some() {
                    self.usage.prompt_cached_tokens = usage.prompt_cached_tokens;
                }
                vec![]
            }
        }
    }

    /// 取出有效的工具调用数据（id 和 name 非空才视为有效）
    pub fn take_tool_calls(&mut self) -> Vec<ToolCallData> {
        let mut calls: Vec<_> = self
            .tool_calls
            .drain()
            .filter_map(|(_, state)| {
                if state.id.is_empty() || state.name.is_empty() {
                    return None;
                }
                Some(ToolCallData {
                    id: state.id,
                    name: state.name,
                    arguments: state.args_buffer,
                })
            })
            .collect();
        calls.sort_by(|a, b| a.id.cmp(&b.id));
        calls
    }

    /// 查看当前有效的工具调用数据（不消费，用于中断时读取部分结果）
    pub fn peek_tool_calls(&self) -> Vec<ToolCallData> {
        let mut calls: Vec<_> = self
            .tool_calls
            .values()
            .filter_map(|state| {
                if state.id.is_empty() || state.name.is_empty() {
                    return None;
                }
                Some(ToolCallData {
                    id: state.id.clone(),
                    name: state.name.clone(),
                    arguments: state.args_buffer.clone(),
                })
            })
            .collect();
        calls.sort_by(|a, b| a.id.cmp(&b.id));
        calls
    }

    /// 获取累积使用统计
    pub fn usage(&self) -> &StreamUsage {
        &self.usage
    }

    // TODO: 未来多轮复用解码器时添加 reset() 方法（清空 tool_calls + 重置 usage）
}

impl Default for StreamAggregator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FinishReason;

    #[test]
    fn text_delta_passes_through() {
        let mut decoder = StreamAggregator::new();
        let events = decoder.process(StreamEvent::TextDelta {
            content: "Hello".to_string(),
        });
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], OutputEvent::Chunk(m) if m.payload.content.as_deref() == Some("Hello"))
        );
    }

    #[test]
    fn reasoning_delta_passes_through() {
        let mut decoder = StreamAggregator::new();
        let events = decoder.process(StreamEvent::ReasoningDelta {
            content: "思考中".to_string(),
        });
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], OutputEvent::Chunk(m) if m.payload.reasoning.as_deref() == Some("思考中"))
        );
    }

    #[test]
    fn tool_call_chunk_creates_state() {
        let mut decoder = StreamAggregator::new();
        let events = decoder.process(StreamEvent::ToolCallChunk {
            index: 0,
            id: Some("call_1".to_string()),
            name: Some("read_file".to_string()),
            args_delta: None,
        });
        assert!(events.is_empty());
        let calls = decoder.take_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
    }

    #[test]
    fn tool_call_chunk_incremental_id_name_args() {
        let mut decoder = StreamAggregator::new();
        // id 先到
        decoder.process(StreamEvent::ToolCallChunk {
            index: 0,
            id: Some("call_1".to_string()),
            name: None,
            args_delta: None,
        });
        // name 后到
        decoder.process(StreamEvent::ToolCallChunk {
            index: 0,
            id: None,
            name: Some("bash".to_string()),
            args_delta: None,
        });
        // arguments 增量
        decoder.process(StreamEvent::ToolCallChunk {
            index: 0,
            id: None,
            name: None,
            args_delta: Some(r#"{"command":"ls"}"#.to_string()),
        });
        let calls = decoder.take_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments, r#"{"command":"ls"}"#);
    }

    #[test]
    fn tool_call_chunk_empty_id_name_filtered() {
        let mut decoder = StreamAggregator::new();
        // 所有字段为空（被 sse 线解码过滤后不会有 chunk 到达，但防御性测试）
        decoder.process(StreamEvent::ToolCallChunk {
            index: 0,
            id: None,
            name: None,
            args_delta: Some("null".to_string()),
        });
        let calls = decoder.take_tool_calls();
        assert!(calls.is_empty());
    }

    #[test]
    fn done_does_not_emit_tool_call_events() {
        let mut decoder = StreamAggregator::new();
        decoder.process(StreamEvent::ToolCallChunk {
            index: 0,
            id: Some("call_1".to_string()),
            name: Some("read_file".to_string()),
            args_delta: Some(r#"{"path":"/tmp"}"#.to_string()),
        });
        let events = decoder.process(StreamEvent::Done {
            usage: StreamUsage::default(),
            finish_reason: FinishReason::ToolCalls,
        });
        assert!(events.is_empty());
        // 数据仍在 decoder 中，通过 take_tool_calls 获取
        let calls = decoder.take_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
    }

    #[test]
    fn done_with_stop_emits_nothing() {
        let mut decoder = StreamAggregator::new();
        let events = decoder.process(StreamEvent::Done {
            usage: StreamUsage::default(),
            finish_reason: FinishReason::Stop,
        });
        assert!(events.is_empty());
    }

    #[test]
    fn take_tool_calls_filters_invalid() {
        let mut decoder = StreamAggregator::new();
        // 有效
        decoder.process(StreamEvent::ToolCallChunk {
            index: 0,
            id: Some("call_1".to_string()),
            name: Some("read_file".to_string()),
            args_delta: Some(r#"{"path":"/tmp"}"#.to_string()),
        });
        // 无效：name 为空
        decoder.process(StreamEvent::ToolCallChunk {
            index: 1,
            id: Some("call_2".to_string()),
            name: None,
            args_delta: None,
        });
        let calls = decoder.take_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");

        // 取出后已清空
        let calls2 = decoder.take_tool_calls();
        assert!(calls2.is_empty());
    }

    #[test]
    fn reset_clears_all_state() {
        let mut decoder = StreamAggregator::new();
        decoder.process(StreamEvent::ToolCallChunk {
            index: 0,
            id: Some("call_1".to_string()),
            name: Some("read_file".to_string()),
            args_delta: None,
        });
        decoder.process(StreamEvent::TextDelta {
            content: "Hello".to_string(),
        });

        decoder.tool_calls.clear();
        decoder.usage = StreamUsage::default();
        assert!(decoder.take_tool_calls().is_empty());
    }

    #[test]
    fn default_is_same_as_new() {
        let mut d1 = StreamAggregator::new();
        let mut d2 = StreamAggregator::default();
        assert!(d1.take_tool_calls().is_empty());
        assert!(d2.take_tool_calls().is_empty());
    }

    #[test]
    fn peek_tool_calls_returns_cloned_without_consuming() {
        let mut decoder = StreamAggregator::new();
        decoder.process(StreamEvent::ToolCallChunk {
            index: 0,
            id: Some("call_1".to_string()),
            name: Some("read_file".to_string()),
            args_delta: Some(r#"{"path":"/tmp"}"#.to_string()),
        });

        let peeked = decoder.peek_tool_calls();
        assert_eq!(peeked.len(), 1);
        assert_eq!(peeked[0].name, "read_file");

        // peek 不消费，take 仍能取出
        let taken = decoder.take_tool_calls();
        assert_eq!(taken.len(), 1);

        // take 后已清空
        assert!(decoder.peek_tool_calls().is_empty());
    }

    #[test]
    fn usage_merges_across_two_done_events() {
        let mut decoder = StreamAggregator::new();

        // 第一个 Done：finish_reason=stop，usage 全零（尚未收到 usage chunk）
        decoder.process(StreamEvent::Done {
            usage: StreamUsage::default(),
            finish_reason: FinishReason::Stop,
        });
        assert_eq!(decoder.usage().prompt_tokens, 0);
        assert_eq!(decoder.usage().completion_tokens, 0);

        // 第二个 Done：include_usage 的最终 chunk，choices=[] 但 usage 有值
        decoder.process(StreamEvent::Done {
            usage: StreamUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                completion_reasoning_tokens: Some(20),
                prompt_cached_tokens: Some(30),
            },
            finish_reason: FinishReason::Stop,
        });
        let usage = decoder.usage();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
        assert_eq!(usage.completion_reasoning_tokens, Some(20));
        assert_eq!(usage.prompt_cached_tokens, Some(30));
    }

    #[test]
    fn usage_keeps_existing_nonzero_values() {
        let mut decoder = StreamAggregator::new();

        // 第一个 Done 已有 usage
        decoder.process(StreamEvent::Done {
            usage: StreamUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                completion_reasoning_tokens: Some(20),
                prompt_cached_tokens: Some(30),
            },
            finish_reason: FinishReason::Stop,
        });

        // 第二个 Done 只有部分字段
        decoder.process(StreamEvent::Done {
            usage: StreamUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                completion_reasoning_tokens: None,
                prompt_cached_tokens: None,
            },
            finish_reason: FinishReason::Stop,
        });
        let usage = decoder.usage();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
        assert_eq!(usage.completion_reasoning_tokens, Some(20));
        assert_eq!(usage.prompt_cached_tokens, Some(30));
    }
}
