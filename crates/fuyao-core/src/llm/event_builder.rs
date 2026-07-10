//! 事件构建器
//!
//! 将 LLM 返回结果转换为 OutputEvent 并通过 EventEmitter 统一推送。

use crate::engine::EventEmitter;
use fuyao_api::message::output::{AssistantMessage, AssistantPayload, ToolCallPayload};
use fuyao_api::message::{EventBase, OutputEvent};
use fuyao_provider::{StreamUsage, ToolCallData};

/// 构建并发出完整的 AssistantMessage 事件
pub(crate) async fn emit_assistant_message(
    emitter: &EventEmitter,
    text: &str,
    reasoning: &str,
    tool_calls_data: &[ToolCallData],
    finish_reason: &str,
    usage: &StreamUsage,
) {
    let tool_calls = if tool_calls_data.is_empty() {
        None
    } else {
        Some(
            tool_calls_data
                .iter()
                .map(|tc| {
                    let args: serde_json::Value = match serde_json::from_str(&tc.arguments) {
                        Ok(v) => v,
                        Err(_) => {
                            let raw: String = tc.arguments.chars().take(200).collect();
                            tracing::warn!(tool = %tc.name, raw = %raw, "工具参数 JSON 解析失败");
                            serde_json::Value::Null
                        }
                    };
                    ToolCallPayload {
                        tool_call_id: tc.id.clone(),
                        tool_name: tc.name.clone(),
                        tool_args: args,
                    }
                })
                .collect(),
        )
    };

    let mut base = EventBase::default();
    base.update_timestamp();

    let event = OutputEvent::Assistant(AssistantMessage {
        base,
        payload: AssistantPayload {
            content: if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            },
            reasoning: if reasoning.is_empty() {
                None
            } else {
                Some(reasoning.to_string())
            },
            tool_calls,
            finish_reason: Some(finish_reason.to_string()),
            completion_tokens: usage.completion_tokens as i64,
            prompt_tokens: usage.prompt_tokens as i64,
            total_tokens: usage.total_tokens as i64,
            reasoning_tokens: usage.completion_reasoning_tokens.unwrap_or(0) as i64,
            cached_tokens: usage.prompt_cached_tokens.unwrap_or(0) as i64,
        },
    });

    let _ = crate::dispatch::dispatch(event, None, emitter).await;
}
