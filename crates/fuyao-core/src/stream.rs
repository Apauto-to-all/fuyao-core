//! LLM 流式会话
//!
//! 消费 provider 的 StreamEvent 流，经 StreamDecoder 解码成 OutputEvent 发出。
//! 流结束后返回累积结果（含 tool_calls）。
//!
//! 失败直接报错：provider 报任何错误都发不可恢复 Error 事件并返回 Err，
//! 不做重试退避（重试后续再加，当前不影响引擎重构）。

use crate::dispatch;
use crate::emit::Emitter;
use crate::interrupt::SharedTurnState;
use fuyao_api::message::output::{ErrorMessage, ErrorPayload};
use fuyao_api::message::{EventBase, OutputEvent};
use fuyao_hooks::SharedHooks;
use fuyao_provider::{
    BoxStream, ChatRequest, Provider, StreamDecoder, StreamError, StreamEvent, StreamOptions,
    StreamUsage, ToolCallData,
};
use std::sync::Arc;

/// 流式会话的结果（一轮 LLM 调用的产出）
pub(crate) struct StreamResult {
    /// 累积的文本内容
    pub text: String,
    /// 累积的推理内容
    pub reasoning: String,
    /// 累积的工具调用（从 decoder.take_tool_calls 取出）
    pub tool_calls: Vec<ToolCallData>,
    /// 本轮用量统计（由模型在流结束时给出）
    pub usage: StreamUsage,
}

/// 运行一次流式 LLM 调用
///
/// 流式期间边吐 Chunk 事件边累积内容；流结束返回累积结果（含 tool_calls）。
/// provider 报错 → 发不可恢复 Error 事件 → 返回 Err（不重试）。
///
/// `state` 共享给中断分支读部分结果——每收到事件就同步累积（block scope 锁，不跨 await）。
///
/// 中断靠外层 select! drop 本函数的 future——本函数自身不知道被中断，
/// `state` 保留中断时刻的部分结果供中断分支读取。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_stream_session(
    request: ChatRequest,
    model: &str,
    options: StreamOptions,
    provider: &Arc<dyn Provider>,
    emitter: &Emitter,
    hooks: &SharedHooks,
    decoder: &mut StreamDecoder,
    state: &SharedTurnState,
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

                // 解码成 OutputEvent 并经管道发出（拦截 → 发送 → 观察）
                let output_events = decoder.process(event);
                for ev in output_events {
                    dispatch::dispatch(emitter, hooks, ev, None).await;
                }

                // 同步共享状态（block scope 锁，不跨 await）
                sync_state(state, &text, &reasoning);
            }
            Err(e) => {
                // 不重试：经管道发不可恢复 Error 事件，直接返回
                dispatch::dispatch(
                    emitter,
                    hooks,
                    OutputEvent::Error(ErrorMessage {
                        base: EventBase::default(),
                        payload: ErrorPayload {
                            message: format!("LLM 调用失败: {e}"),
                            recoverable: false,
                        },
                    }),
                    None,
                )
                .await;
                tracing::warn!(session_id = emitter.session_id(), cause = %e, "LLM 流式调用失败");
                return Err(e);
            }
        }
    }

    // 流正常结束：取累积的 tool_calls 与用量统计，同步最终状态，返回
    let tool_calls = decoder.take_tool_calls();
    let usage = decoder.usage().clone();
    sync_state_with_tools(state, &text, &reasoning, &tool_calls);

    Ok(StreamResult {
        text,
        reasoning,
        tool_calls,
        usage,
    })
}

/// 同步共享状态（block scope 锁，不跨 await）
///
/// 中断分支可能在本函数 await 期间读 state，所以每次事件后都同步最新累积的文本/推理。
fn sync_state(state: &SharedTurnState, text: &str, reasoning: &str) {
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    s.text = text.to_string();
    s.reasoning = reasoning.to_string();
}

/// 同步最终状态（流结束时调，含 tool_calls）
fn sync_state_with_tools(
    state: &SharedTurnState,
    text: &str,
    reasoning: &str,
    tool_calls: &[ToolCallData],
) {
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    s.text = text.to_string();
    s.reasoning = reasoning.to_string();
    s.tool_calls = tool_calls.to_vec();
}
