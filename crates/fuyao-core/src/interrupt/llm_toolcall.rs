//! LLM 工具调用流中中断
//!
//! 中断触发时 LLM 正在流式输出工具调用（ToolCallChunk 增量拼接中）。
//! 保留有 name 和 id 的有效工具调用，发出中断专用 ToolResult。

use crate::engine::EventEmitter;
use crate::interrupt;
use crate::llm::event_builder;
use fuyao_api::message::InterruptData;
use fuyao_provider::{StreamUsage, ToolCallData};

/// 处理 LLM 工具调用流中的中断
///
/// 1. 将已累积内容 + 中断有效的工具调用作为 AssistantMessage 发出
/// 2. 为中断有效的工具调用发出中断专用 ToolResult
///
/// 中断输出事件已由 InputDispatcher dispatch 管道发出，此处只负责增量结果。
pub(crate) async fn handle(
    emitter: &EventEmitter,
    data: InterruptData,
    text: &str,
    reasoning: &str,
    tool_calls: &[ToolCallData],
    usage: &StreamUsage,
) {
    // "中断有效"：有 id 和 name 的工具调用
    let valid_calls: Vec<&ToolCallData> = tool_calls
        .iter()
        .filter(|tc| !tc.id.is_empty() && !tc.name.is_empty())
        .collect();

    // 发出 AssistantMessage（含累积内容 + 有效工具调用）
    let has_content = !text.is_empty() || !reasoning.is_empty();
    if has_content || !valid_calls.is_empty() {
        event_builder::emit_assistant_message(
            emitter,
            text,
            reasoning,
            &valid_calls
                .iter()
                .map(|tc| (*tc).clone())
                .collect::<Vec<_>>(),
            "interrupted",
            usage,
        )
        .await;
    }

    // 为每个有效工具调用发出中断专用 ToolResult
    let source = data.source.clone();
    let reason = data.reason.clone();
    for tc in &valid_calls {
        let result = interrupt::make_interrupt_tool_result(
            tc.id.clone(),
            tc.name.clone(),
            source.clone(),
            reason.clone(),
        );
        crate::dispatch::dispatch(
            fuyao_api::message::OutputEvent::ToolResult(result),
            None,
            emitter,
        )
        .await;
    }
}
