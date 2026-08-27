//! Anthropic Messages 协议非流式响应解析（纯函数，不依赖 HTTP）
//!
//! `/v1/messages` 非流式响应 JSON 文本 → 统一 [`ChatResponse`]。入方向宽松
//! 反序列化：全 Option 单结构体吸收字段缺失 / null，content 数组按 block 的
//! `type` 字符串分发（text 拼接为内容、tool_use 收集为工具调用），未知
//! block 类型（thinking 等）静默跳过。

use fuyao_api::ToolCallData;
use serde::Deserialize;
use uuid::Uuid;

use crate::StreamError;
use crate::provider::{ChatResponse, FinishReason, StreamUsage};

/// 非流式响应顶层（content / usage / stop_reason 均可缺失或为 null）
#[derive(Debug, Deserialize)]
struct MessageResponse {
    #[serde(default)]
    content: Option<Vec<MessageContentBlock>>,
    #[serde(default)]
    usage: Option<MessageUsage>,
    #[serde(default)]
    stop_reason: Option<String>,
}

/// content block 宽松结构：text / tool_use / thinking 等共用一个结构，
/// `type` 字段决定分发去向，未列出的字段（thinking / signature 等）忽略
#[derive(Debug, Deserialize)]
struct MessageContentBlock {
    #[serde(rename = "type")]
    block_type: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    /// 工具入参（JSON object 原文）
    #[serde(default)]
    input: Option<serde_json::Value>,
}

/// usage（非流式四个值同处到达：输入三桶 + 输出累计值）
#[derive(Debug, Deserialize)]
struct MessageUsage {
    #[serde(default)]
    input_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    output_tokens: Option<u32>,
}

/// 解析非流式完整响应
///
/// 失败条件：非法 JSON、content 数组缺失 / null / 非数组，均归
/// [`StreamError::StreamParseError`]。
pub fn parse_completion(body: &str) -> Result<ChatResponse, StreamError> {
    let resp: MessageResponse = serde_json::from_str(body)
        .map_err(|e| StreamError::StreamParseError(format!("非流式响应反序列化失败: {e}")))?;
    let Some(blocks) = resp.content else {
        return Err(StreamError::StreamParseError(
            "响应中无 content 数组".to_string(),
        ));
    };

    let mut text = String::new();
    let mut tool_calls: Vec<ToolCallData> = Vec::new();
    for block in blocks {
        match block.block_type.as_deref() {
            // text 块拼接为内容（空文本无贡献）
            Some("text") => {
                if let Some(t) = block.text {
                    text.push_str(&t);
                }
            }
            // tool_use 块收集为类型化工具调用：input（JSON object）序列化回字符串，
            // id 缺失或为空时生成 UUID 兜底标识（保证与既有 id 不冲突）
            Some("tool_use") => tool_calls.push(ToolCallData {
                id: block
                    .id
                    .filter(|id| !id.is_empty())
                    .unwrap_or_else(|| Uuid::new_v4().to_string()),
                name: block.name.unwrap_or_default(),
                arguments: match block.input {
                    Some(value @ serde_json::Value::Object(_)) => value.to_string(),
                    _ => "{}".to_string(),
                },
            }),
            // thinking 等其余类型不参与内容与工具调用
            _ => {}
        }
    }

    // usage 四值同处到达：输入三桶 saturating 求和为总 prompt、缓存读 / 写
    // 桶各自留痕；usage 整体缺失时归零值，不合成缓存桶
    let usage = match resp.usage {
        Some(u) => {
            let input_tokens = u.input_tokens.unwrap_or(0);
            let cache_read = u.cache_read_input_tokens.unwrap_or(0);
            let cache_creation = u.cache_creation_input_tokens.unwrap_or(0);
            let output_tokens = u.output_tokens.unwrap_or(0);
            let input_total = input_tokens
                .saturating_add(cache_read)
                .saturating_add(cache_creation);
            StreamUsage {
                prompt_tokens: input_total,
                completion_tokens: output_tokens,
                total_tokens: input_total.saturating_add(output_tokens),
                completion_reasoning_tokens: None,
                prompt_cached_tokens: Some(cache_read),
                prompt_cache_creation_tokens: Some(cache_creation),
            }
        }
        None => StreamUsage::default(),
    };

    // stop_reason 宽松映射：end_turn→Stop、tool_use→ToolCalls、max_tokens→Length、
    // 其余与缺省→Stop
    let finish_reason = match resp.stop_reason.as_deref() {
        Some("tool_use") => FinishReason::ToolCalls,
        Some("max_tokens") => FinishReason::Length,
        _ => FinishReason::Stop,
    };

    Ok(ChatResponse {
        content: (!text.is_empty()).then_some(text),
        reasoning: None,
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        usage,
        finish_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 纯文本响应：text 拼接为 content，无工具调用
    #[test]
    fn text_only_response_parses() {
        let resp = parse_completion(
            r#"{
                "id": "msg_01",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "你好"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }"#,
        )
        .unwrap();
        assert_eq!(resp.content.as_deref(), Some("你好"));
        assert!(resp.tool_calls.is_none());
        assert!(resp.reasoning.is_none());
        assert_eq!(resp.finish_reason, FinishReason::Stop);
        assert_eq!(resp.usage.prompt_tokens, 10);
        assert_eq!(resp.usage.completion_tokens, 5);
        assert_eq!(resp.usage.total_tokens, 15);
    }

    /// 文本 + 多工具调用：thinking 块跳过、input 序列化回字符串、id 缺失生成兜底标识
    #[test]
    fn text_and_multiple_tool_calls() {
        let resp = parse_completion(
            r#"{
                "content": [
                    {"type": "thinking", "thinking": "内心独白"},
                    {"type": "text", "text": "先查天气"},
                    {"type": "tool_use", "id": "toolu_01", "name": "get_weather", "input": {"city": "北京"}},
                    {"type": "tool_use", "name": "read_file", "input": {"path": "a.rs"}}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 3, "output_tokens": 7}
            }"#,
        )
        .unwrap();
        assert_eq!(resp.content.as_deref(), Some("先查天气"));
        let calls = resp.tool_calls.expect("应有两个工具调用");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "toolu_01");
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].arguments, r#"{"city":"北京"}"#);
        // id 缺失：生成非空兜底标识，且与已见 id 不冲突
        assert!(!calls[1].id.is_empty());
        assert_ne!(calls[1].id, calls[0].id);
        assert_eq!(calls[1].name, "read_file");
        assert_eq!(calls[1].arguments, r#"{"path":"a.rs"}"#);
        assert_eq!(resp.finish_reason, FinishReason::ToolCalls);
    }

    /// usage 三桶归一：prompt_tokens 三桶 saturating 求和，缓存桶各自留痕
    #[test]
    fn usage_three_buckets_normalize() {
        let resp = parse_completion(
            r#"{
                "content": [{"type": "text", "text": "缓存命中"}],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": 4,
                    "cache_read_input_tokens": 152736,
                    "cache_creation_input_tokens": 4200,
                    "output_tokens": 27
                }
            }"#,
        )
        .unwrap();
        assert_eq!(resp.usage.prompt_tokens, 156940, "三桶求和 4+152736+4200");
        assert_eq!(resp.usage.prompt_cached_tokens, Some(152736));
        assert_eq!(resp.usage.prompt_cache_creation_tokens, Some(4200));
        assert_eq!(resp.usage.completion_tokens, 27);
        assert_eq!(resp.usage.total_tokens, 156967);
    }

    /// stop_reason 宽松映射：end_turn→Stop、tool_use→ToolCalls、max_tokens→Length、
    /// 未知与缺省→Stop
    #[test]
    fn stop_reason_loose_mapping() {
        for (reason, expected) in [
            ("end_turn", FinishReason::Stop),
            ("tool_use", FinishReason::ToolCalls),
            ("max_tokens", FinishReason::Length),
            ("stop_sequence", FinishReason::Stop),
        ] {
            let body = format!(
                r#"{{"content":[{{"type":"text","text":"ok"}}],"stop_reason":"{reason}"}}"#
            );
            let resp = parse_completion(&body).unwrap();
            assert_eq!(resp.finish_reason, expected, "stop_reason={reason}");
        }
        // 缺省 → Stop
        let resp = parse_completion(r#"{"content":[{"type":"text","text":"ok"}]}"#).unwrap();
        assert_eq!(resp.finish_reason, FinishReason::Stop);
    }

    /// 非法 JSON → StreamParseError
    #[test]
    fn invalid_json_rejected() {
        assert!(matches!(
            parse_completion("not json"),
            Err(StreamError::StreamParseError(_))
        ));
    }

    /// content 数组缺失 → StreamParseError
    #[test]
    fn missing_content_rejected() {
        assert!(matches!(
            parse_completion(r#"{"usage":{"input_tokens":1}}"#),
            Err(StreamError::StreamParseError(_))
        ));
    }

    /// text 全空则 content 为 None；usage 缺失归零值，不合成缓存桶
    #[test]
    fn blank_text_and_missing_usage_default() {
        let resp = parse_completion(
            r#"{"content":[{"type":"text","text":""},{"type":"text","text":""}]}"#,
        )
        .unwrap();
        assert!(resp.content.is_none());
        assert!(resp.tool_calls.is_none());
        assert_eq!(resp.usage.prompt_tokens, 0);
        assert_eq!(resp.usage.total_tokens, 0);
        assert_eq!(resp.usage.prompt_cached_tokens, None);
        assert_eq!(resp.finish_reason, FinishReason::Stop);
    }
}
