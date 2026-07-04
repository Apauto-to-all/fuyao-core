//! LLM 流式输出中中断
//!
//! 中断触发时 LLM 正在流式输出文本/推理内容。
//! 停止 LLM 流，将已累积的增量助手消息发出，避免内容丢失。

use crate::engine::EventEmitter;
use crate::llm::event_builder;
use fuyao_provider::StreamUsage;

/// 处理 LLM 流式输出中的中断
///
/// 将已累积的文本/推理内容作为 AssistantMessage（finish_reason = "interrupted"）发出。
/// 中断输出事件已由 InputDispatcher dispatch 管道发出，此处只负责增量结果。
pub(crate) async fn handle(
    emitter: &EventEmitter,
    _data: fuyao_api::message::InterruptData,
    text: &str,
    reasoning: &str,
    usage: &StreamUsage,
) {
    let has_content = !text.is_empty() || !reasoning.is_empty();

    if has_content {
        event_builder::emit_assistant_message(emitter, text, reasoning, &[], "interrupted", usage)
            .await;
    }
}
