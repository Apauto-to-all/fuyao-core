//! LLM 流式会话
//!
//! 消费 provider 的 StreamEvent 流，经 StreamDecoder 解码成 OutputEvent 发出。
//! 第二步不含重试/退避——provider 报错直接发 Error 事件并返回 Err。
//! 重试退避留给第三步（和中断机制一起实现）。

use crate::emit::emit_event;
use fuyao_api::message::output::{ErrorMessage, ErrorPayload};
use fuyao_api::message::{EventBase, OutputEvent};
use fuyao_provider::{
    BoxStream, ChatRequest, Provider, StreamDecoder, StreamError, StreamEvent, StreamOptions,
};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

/// 流式会话的结果（一轮 LLM 调用的产出）
pub(crate) struct StreamResult {
    /// 累积的文本内容
    pub text: String,
    /// 累积的推理内容
    pub reasoning: String,
}

/// 运行一次流式 LLM 调用
///
/// 流式期间边吐 Chunk 事件边累积内容；流结束返回累积结果。
/// provider 报错 → 发 Error 事件（recoverable=false）→ 返回 Err（第二步不重试）。
///
/// `session_id` 会盖到每个发出事件的 base.session_id 上（全程标签）。
pub(crate) async fn run_stream_session(
    request: ChatRequest,
    model: &str,
    options: StreamOptions,
    provider: &Arc<dyn Provider>,
    tx_event: &Sender<OutputEvent>,
    session_id: &str,
    decoder: &mut StreamDecoder,
) -> Result<StreamResult, StreamError> {
    let mut stream: BoxStream<Result<StreamEvent, StreamError>> =
        provider.stream_chat(request, model, options);

    let mut text = String::new();
    let mut reasoning = String::new();

    use futures_util::StreamExt;
    while let Some(result) = stream.next().await {
        match result {
            Ok(event) => {
                // 累积内容（用于流结束后组装 AssistantMessage）
                match &event {
                    StreamEvent::TextDelta { content } => text.push_str(content),
                    StreamEvent::ReasoningDelta { content } => reasoning.push_str(content),
                    _ => {}
                }

                // 解码成 OutputEvent 并发出（覆盖 session_id）
                let output_events = decoder.process(event);
                for ev in output_events {
                    emit_event(tx_event, session_id, ev).await;
                }
            }
            Err(e) => {
                // 第二步不重试：发不可恢复 Error 事件，直接返回
                let error_event = OutputEvent::Error(ErrorMessage {
                    base: EventBase::default(),
                    payload: ErrorPayload {
                        message: format!("LLM 调用失败: {e}"),
                        recoverable: false,
                    },
                });
                emit_event(tx_event, session_id, error_event).await;
                tracing::warn!(session_id = session_id, cause = %e, "LLM 流式调用失败");
                return Err(e);
            }
        }
    }

    Ok(StreamResult { text, reasoning })
}
