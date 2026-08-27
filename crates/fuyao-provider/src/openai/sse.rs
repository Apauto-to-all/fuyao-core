//! SSE 流式线解码（纯函数，不依赖 HTTP）
//!
//! 把 OpenAI 兼容 `/chat/completions` 流式响应的字节流解码为统一的
//! [`StreamEvent`]。与传输层解耦：输入是单条 SSE 行 / 反序列化后的 chunk JSON，
//! 输出是引擎层消费的事件，可独立单测。
//!
//! 注意：本模块只做「线解码」（SSE 文本 → StreamEvent）；工具调用增量拼接 /
//! usage 累加 / OutputEvent 组装由 [`crate::stream_decoder`] 的累加器负责，
//! 两者职责正交。

use crate::StreamError;
use crate::provider::{FinishReason as ProviderFinishReason, StreamEvent, StreamUsage};

/// 解析 SSE 行，返回反序列化后的 chunk
///
/// SSE 格式：`data: {json}\n` 或 `data: [DONE]\n`
pub(crate) fn parse_sse_line(line: &str) -> Result<Option<serde_json::Value>, StreamError> {
    let line = line.trim();

    // 空行或注释行
    if line.is_empty() || line.starts_with(':') {
        return Ok(None);
    }

    // 提取 data 前缀后的内容
    let data = match line.strip_prefix("data:") {
        Some(d) => d.trim(),
        None => return Ok(None),
    };

    // 流结束标记
    if data == "[DONE]" {
        return Ok(None);
    }

    // 反序列化 JSON
    serde_json::from_str(data)
        .map(Some)
        .map_err(|e| StreamError::StreamParseError(format!("SSE JSON 解析失败: {e}")))
}

/// 从 SSE chunk 中提取流式事件
pub(crate) fn extract_stream_events(chunk: &serde_json::Value) -> Vec<StreamEvent> {
    let mut events = Vec::new();

    let choices = match chunk.get("choices").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => {
            // choices 为空或不存在时，检查顶层 usage（include_usage 的最后一个 chunk）
            let usage = extract_usage(chunk);
            if usage.total_tokens > 0 || usage.prompt_tokens > 0 || usage.completion_tokens > 0 {
                events.push(StreamEvent::Done {
                    usage,
                    finish_reason: ProviderFinishReason::Stop,
                });
            }
            return events;
        }
    };

    // choices 为空数组时（include_usage 的最终 chunk：choices=[], usage={...}）
    if choices.is_empty() {
        let usage = extract_usage(chunk);
        if usage.total_tokens > 0 || usage.prompt_tokens > 0 || usage.completion_tokens > 0 {
            events.push(StreamEvent::Done {
                usage,
                finish_reason: ProviderFinishReason::Stop,
            });
        }
        return events;
    }

    for choice in choices {
        let delta = match choice.get("delta") {
            Some(d) => d,
            None => continue,
        };

        // 文本内容增量
        if let Some(content) = delta.get("content").and_then(|c| c.as_str())
            && !content.is_empty()
        {
            events.push(StreamEvent::TextDelta {
                content: content.to_string(),
            });
        }

        // 思考内容增量
        // 兼容两种字段名：reasoning_content（DeepSeek/Qwen）和 reasoning（部分供应商）
        let reasoning = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(|r| r.as_str())
            .unwrap_or("");
        if !reasoning.is_empty() {
            events.push(StreamEvent::ReasoningDelta {
                content: reasoning.to_string(),
            });
        }

        // 工具调用增量（按 index 增量拼接，id/name/arguments 独立到达）
        if let Some(tool_calls) = delta.get("tool_calls").and_then(|tc| tc.as_array()) {
            for tc in tool_calls {
                let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;

                let id = tc
                    .get("id")
                    .and_then(|i| i.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());

                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());

                let args_delta = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());

                if id.is_some() || name.is_some() || args_delta.is_some() {
                    events.push(StreamEvent::ToolCallChunk {
                        index,
                        id,
                        name,
                        args_delta,
                    });
                }
            }
        }

        // 流结束
        if let Some(finish_reason) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            let reason = match finish_reason {
                "stop" => ProviderFinishReason::Stop,
                "tool_calls" => ProviderFinishReason::ToolCalls,
                "length" => ProviderFinishReason::Length,
                _ => ProviderFinishReason::Stop,
            };

            // 从 chunk 顶层提取 usage
            let usage = extract_usage(chunk);
            events.push(StreamEvent::Done {
                usage,
                finish_reason: reason,
            });
        }
    }

    events
}

/// 从 chunk 中提取 usage 统计
pub(crate) fn extract_usage(chunk: &serde_json::Value) -> StreamUsage {
    let usage = match chunk.get("usage") {
        Some(u) => u,
        None => return StreamUsage::default(),
    };

    StreamUsage {
        prompt_tokens: usage
            .get("prompt_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        completion_tokens: usage
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        total_tokens: usage
            .get("total_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        completion_reasoning_tokens: usage
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        prompt_cached_tokens: usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        prompt_cache_creation_tokens: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========== parse_sse_line ===========

    #[test]
    fn parse_sse_line_skips_empty_and_comments() {
        assert!(parse_sse_line("").unwrap().is_none());
        assert!(parse_sse_line(": comment").unwrap().is_none());
        assert!(parse_sse_line("data: [DONE]").unwrap().is_none());
    }

    #[test]
    fn parse_sse_line_parses_json() {
        let chunk = parse_sse_line(r#"data: {"choices":[]}"#).unwrap().unwrap();
        assert!(chunk.get("choices").unwrap().as_array().unwrap().is_empty());
    }

    #[test]
    fn parse_sse_line_rejects_invalid_json() {
        assert!(parse_sse_line("data: {invalid}").is_err());
    }

    #[test]
    fn extract_stream_events_text_delta() {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {"content": "Hello"},
                "finish_reason": null
            }]
        });
        let events = extract_stream_events(&chunk);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::TextDelta { content } if content == "Hello"));
    }

    #[test]
    fn extract_stream_events_reasoning_delta() {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {"reasoning_content": "思考中..."},
                "finish_reason": null
            }]
        });
        let events = extract_stream_events(&chunk);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], StreamEvent::ReasoningDelta { content } if content == "思考中...")
        );
    }

    #[test]
    fn extract_stream_events_reasoning_field_fallback() {
        // 部分供应商使用 "reasoning" 字段名
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {"reasoning": "推理内容"},
                "finish_reason": null
            }]
        });
        let events = extract_stream_events(&chunk);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], StreamEvent::ReasoningDelta { content } if content == "推理内容")
        );
    }

    #[test]
    fn extract_stream_events_tool_call_chunk_with_id_and_name() {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": {"name": "bash", "arguments": ""}
                    }]
                }
            }]
        });
        let events = extract_stream_events(&chunk);
        assert!(!events.is_empty());
        assert!(
            matches!(&events[0], StreamEvent::ToolCallChunk { id, name, args_delta, .. }
                if id.as_deref() == Some("call_1")
                && name.as_deref() == Some("bash")
                && args_delta.is_none())
        );
    }

    #[test]
    fn extract_stream_events_tool_call_chunk_args_only() {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {"arguments": "{\"path\":"}
                    }]
                }
            }]
        });
        let events = extract_stream_events(&chunk);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], StreamEvent::ToolCallChunk { args_delta, id, name, .. }
                if args_delta.as_deref() == Some("{\"path\":")
                && id.is_none()
                && name.is_none())
        );
    }

    #[test]
    fn extract_stream_events_tool_call_chunk_empty_fields_ignored() {
        // 所有字段为空时不应该产生事件
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "",
                        "function": {"name": "", "arguments": ""}
                    }]
                }
            }]
        });
        let events = extract_stream_events(&chunk);
        assert!(events.is_empty());
    }

    #[test]
    fn extract_stream_events_done() {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150
            }
        });
        let events = extract_stream_events(&chunk);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Done {
                usage,
                finish_reason,
            } => {
                assert_eq!(usage.prompt_tokens, 100);
                assert_eq!(usage.completion_tokens, 50);
                assert_eq!(*finish_reason, ProviderFinishReason::Stop);
            }
            _ => panic!("期望 Done 事件"),
        }
    }

    #[test]
    fn extract_stream_events_done_with_tool_calls() {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {},
                "finish_reason": "tool_calls"
            }]
        });
        let events = extract_stream_events(&chunk);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            StreamEvent::Done {
                finish_reason: ProviderFinishReason::ToolCalls,
                ..
            }
        ));
    }

    #[test]
    fn extract_usage_from_chunk() {
        let chunk = serde_json::json!({
            "usage": {
                "prompt_tokens": 200,
                "completion_tokens": 80,
                "total_tokens": 280,
                "completion_tokens_details": {"reasoning_tokens": 30},
                "prompt_tokens_details": {"cached_tokens": 50}
            }
        });
        let usage = extract_usage(&chunk);
        assert_eq!(usage.prompt_tokens, 200);
        assert_eq!(usage.completion_tokens, 80);
        assert_eq!(usage.total_tokens, 280);
        assert_eq!(usage.completion_reasoning_tokens, Some(30));
        assert_eq!(usage.prompt_cached_tokens, Some(50));
    }

    #[test]
    fn extract_usage_defaults_when_missing() {
        let chunk = serde_json::json!({});
        let usage = extract_usage(&chunk);
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 0);
    }

    #[test]
    fn extract_usage_defaults_when_null_fields() {
        let chunk = serde_json::json!({
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5
            }
        });
        let usage = extract_usage(&chunk);
        assert_eq!(usage.prompt_tokens, 10);
        assert!(usage.completion_reasoning_tokens.is_none());
        assert!(usage.prompt_cached_tokens.is_none());
    }

    #[test]
    fn extract_stream_events_empty_choices_with_usage() {
        // include_usage 的最终 chunk：choices 为空数组，usage 在顶层
        let chunk = serde_json::json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150,
                "completion_tokens_details": {"reasoning_tokens": 20},
                "prompt_tokens_details": {"cached_tokens": 30}
            }
        });
        let events = extract_stream_events(&chunk);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Done {
                usage,
                finish_reason,
            } => {
                assert_eq!(usage.prompt_tokens, 100);
                assert_eq!(usage.completion_tokens, 50);
                assert_eq!(usage.total_tokens, 150);
                assert_eq!(usage.completion_reasoning_tokens, Some(20));
                assert_eq!(usage.prompt_cached_tokens, Some(30));
                assert_eq!(*finish_reason, ProviderFinishReason::Stop);
            }
            _ => panic!("期望 Done 事件"),
        }
    }

    #[test]
    fn extract_stream_events_empty_choices_without_usage() {
        // choices 为空且无 usage → 不发任何事件
        let chunk = serde_json::json!({
            "choices": []
        });
        let events = extract_stream_events(&chunk);
        assert!(events.is_empty());
    }

    #[test]
    fn extract_stream_events_no_choices_with_usage() {
        // choices 字段不存在但有 usage
        let chunk = serde_json::json!({
            "usage": {
                "prompt_tokens": 50,
                "completion_tokens": 25,
                "total_tokens": 75
            }
        });
        let events = extract_stream_events(&chunk);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Done { usage, .. } => {
                assert_eq!(usage.prompt_tokens, 50);
                assert_eq!(usage.completion_tokens, 25);
            }
            _ => panic!("期望 Done 事件"),
        }
    }
}
