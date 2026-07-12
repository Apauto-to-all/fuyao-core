//! fuyao-core 集成测试共享 fixture
//!
//! 可参数化的 MockProvider（复用 stream_session.rs:287 的 stream::iter 模式），
//! 以及 AgentContext / 事件收集辅助。

// 跨测试二进制共享：未用部分不报 dead_code
#![allow(dead_code)]

use futures_util::stream;
use fuyao_api::AgentContext;
use fuyao_provider::{
    BoxStream, ChatRequest, ChatResponse, FinishReason, Provider, StreamError, StreamEvent,
    StreamOptions, StreamUsage,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// 可参数化的 Mock Provider
///
/// 支持两种模式：
/// - **固定序列**：`responses` 仅一项时，每次调用返回同一序列（用于纯文本单轮测试）
/// - **状态化序列**：`responses` 含多项时，第 N 次调用返回第 N 项（用于工具调用 ReAct
///   循环：首次返回 ToolCallChunk，第二次返回纯文本收敛到 Stop，避免无限循环）
pub struct MockProvider {
    /// 按调用次数依次返回的事件序列（超出范围时重复最后一项）
    pub responses: Arc<Vec<Vec<StreamEvent>>>,
    /// 调用计数（用于断言 LLM 被调用了几次）
    pub call_count: Arc<AtomicUsize>,
}

impl MockProvider {
    /// 单一固定序列（每次调用返回同一序列）
    pub fn fixed(events: Vec<StreamEvent>) -> Self {
        Self {
            responses: Arc::new(vec![events]),
            call_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 状态化序列：第 N 次调用返回第 N 项
    pub fn sequenced(responses: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            responses: Arc::new(responses),
            call_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl Provider for MockProvider {
    fn stream_chat(
        &self,
        _request: ChatRequest,
        _model: &str,
        _options: StreamOptions,
    ) -> BoxStream<Result<StreamEvent, StreamError>> {
        let n = self.call_count.fetch_add(1, Ordering::SeqCst);
        // 超出范围时取最后一项（固定序列模式下 responses.len()==1）
        let idx = n.min(self.responses.len() - 1);
        let events: Vec<Result<StreamEvent, StreamError>> =
            self.responses[idx].iter().cloned().map(Ok).collect();
        Box::pin(stream::iter(events))
    }

    async fn chat(&self, _request: ChatRequest, _model: &str) -> Result<ChatResponse, StreamError> {
        Err(StreamError::ApiError("mock: chat 不支持".into()))
    }
}

/// 构造测试用 AgentContext（model_id 含短名）
pub fn test_agent_ctx() -> AgentContext {
    let mut ctx = AgentContext::default();
    ctx.model_config.model_id = Some("test-model".to_string());
    ctx
}

/// 构造纯文本回复事件序列（单轮即结束）
pub fn text_events(content: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta {
            content: content.to_string(),
        },
        StreamEvent::Done {
            usage: StreamUsage::default(),
            finish_reason: FinishReason::Stop,
        },
    ]
}

/// 构造含单个工具调用的事件序列（触发 ReAct 工具执行）
pub fn tool_call_events(tool_name: &str, args: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCallChunk {
            index: 0,
            id: Some("tc1".into()),
            name: Some(tool_name.to_string()),
            args_delta: Some(args.to_string()),
        },
        StreamEvent::Done {
            usage: StreamUsage::default(),
            finish_reason: FinishReason::ToolCalls,
        },
    ]
}

/// 在超时窗口内收集 OutputEvent，直到超时或收够 max_events 条
pub async fn collect_events(
    handle: &fuyao_core::EngineHandle,
    max_events: usize,
    timeout_ms: u64,
) -> Vec<fuyao_api::message::OutputEvent> {
    let mut events = Vec::new();
    for _ in 0..max_events {
        match tokio::time::timeout(Duration::from_millis(timeout_ms), handle.next_event()).await {
            Ok(Some(e)) => events.push(e),
            _ => break,
        }
    }
    events
}
