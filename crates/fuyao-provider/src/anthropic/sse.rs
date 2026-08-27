//! Anthropic Messages 协议 SSE 流式线解码（有状态解码器，不依赖 HTTP）
//!
//! 把 `/v1/messages` 流式响应的 SSE 行序列解码为统一的 [`StreamEvent`]。
//! 与传输层解耦：输入是单条 SSE 行，输出是引擎层消费的事件，可独立单测。
//!
//! 线形态：`event: 类型` 行 + `data: {json}` 行，只认 `data:` 行——事件类型
//! 从 JSON 的 `type` 字段取，`event:` 行忽略。该协议无 `[DONE]` 终止标记，
//! 流结束以 `message_stop` 事件为准。
//!
//! 跨事件状态的两个来源：
//! - usage 分两处到达：`message_start` 带输入三桶、`message_delta` 带输出
//!   累计值，`message_stop` 时归一成单个 Done 事件
//! - 消息级 content block 序号映射为工具调用序号（仅 tool_use block 计数，
//!   text / thinking 等其余 block 不占序号），供 input_json_delta 回填归属
//!
//! 工具参数增量拼接、文本拼接由消费端 [`crate::stream_decoder`] 的累加器
//! 负责，本模块只做线解码，两者职责正交。

use crate::StreamError;
use crate::provider::{FinishReason, StreamEvent, StreamUsage};
use std::collections::HashMap;

/// Anthropic SSE 流式线解码器
///
/// 每个流式响应对应一个实例，逐行喂入 [`AnthropicStreamDecoder::feed_line`]。
pub struct AnthropicStreamDecoder {
    /// 输入三桶之一：最后一个缓存断点之后的残余输入
    input_tokens: u32,
    /// 输入三桶之一：缓存命中读
    cache_read_input_tokens: u32,
    /// 输入三桶之一：缓存写入
    cache_creation_input_tokens: u32,
    /// 输出 token 累计值（message_delta 覆盖式更新，API 语义是累计值）
    output_tokens: u32,
    /// 记录中的完成原因，message_stop 时随 Done 发出
    finish_reason: FinishReason,
    /// 下一个工具调用序号（已见 tool_use block 的计数）
    next_tool_index: usize,
    /// 消息级 content block 序号 → 工具调用序号（仅 tool_use block 登记）
    block_tool_indexes: HashMap<u64, usize>,
}

impl AnthropicStreamDecoder {
    /// 创建解码器
    pub fn new() -> Self {
        Self {
            input_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            output_tokens: 0,
            finish_reason: FinishReason::Stop,
            next_tool_index: 0,
            block_tool_indexes: HashMap::new(),
        }
    }

    /// 喂入单条 SSE 行，返回该行产出的流事件
    ///
    /// 空行 / 注释行（`:` 开头）/ 非 `data:` 行无产出；`data:` 载荷 JSON
    /// 解析失败的行跳过不报错（容错）。
    pub fn feed_line(&mut self, line: &str) -> Result<Vec<StreamEvent>, StreamError> {
        let Some(data) = data_payload(line) else {
            return Ok(Vec::new());
        };
        // 容错：解析失败的行跳过不报错
        let Ok(event) = serde_json::from_str::<serde_json::Value>(data) else {
            return Ok(Vec::new());
        };
        self.dispatch(&event)
    }

    /// 按 JSON `type` 字段分发事件（`event:` 行的类型信息不参与分发）
    fn dispatch(&mut self, event: &serde_json::Value) -> Result<Vec<StreamEvent>, StreamError> {
        let mut out = Vec::new();
        match event
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
        {
            "message_start" => self.record_input_usage(&event["message"]["usage"]),
            "content_block_start" => out.extend(self.tool_call_start(event)),
            "content_block_delta" => out.extend(self.content_block_delta(event)),
            "message_delta" => self.record_message_delta(event),
            "message_stop" => out.push(self.done_event()),
            "error" => return Err(error_to_stream_error(event)),
            // ping / content_block_stop / 未知类型：无动作
            _ => {}
        }
        Ok(out)
    }

    /// message_start：收 message.usage 的输入三桶
    fn record_input_usage(&mut self, usage: &serde_json::Value) {
        self.input_tokens = u64_field(usage, "input_tokens") as u32;
        self.cache_read_input_tokens = u64_field(usage, "cache_read_input_tokens") as u32;
        self.cache_creation_input_tokens = u64_field(usage, "cache_creation_input_tokens") as u32;
    }

    /// content_block_start：tool_use block 发 id/name 首达增量并登记
    /// block 序号 → 工具序号映射；text / thinking 等其余 block 不占工具序号
    fn tool_call_start(&mut self, event: &serde_json::Value) -> Vec<StreamEvent> {
        let block = &event["content_block"];
        if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
            return Vec::new();
        }
        let tool_index = self.next_tool_index;
        self.next_tool_index += 1;
        self.block_tool_indexes
            .insert(u64_field(event, "index"), tool_index);
        vec![StreamEvent::ToolCallChunk {
            index: tool_index,
            id: block.get("id").and_then(|v| v.as_str()).map(str::to_string),
            name: block
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            args_delta: None,
        }]
    }

    /// content_block_delta：text_delta 转文本增量；thinking_delta 转思考增量；
    /// input_json_delta 按映射转对应工具序号的参数分片增量；
    /// signature_delta 与未知类型静默忽略（signature 无落点）
    fn content_block_delta(&self, event: &serde_json::Value) -> Vec<StreamEvent> {
        let delta = &event["delta"];
        let delta_type = delta
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or_default();
        match delta_type {
            "text_delta" => match delta.get("text").and_then(|t| t.as_str()) {
                Some(text) if !text.is_empty() => vec![StreamEvent::TextDelta {
                    content: text.to_string(),
                }],
                _ => Vec::new(),
            },
            "thinking_delta" => match delta.get("thinking").and_then(|t| t.as_str()) {
                Some(text) if !text.is_empty() => vec![StreamEvent::ReasoningDelta {
                    content: text.to_string(),
                }],
                _ => Vec::new(),
            },
            "input_json_delta" => {
                let partial = delta
                    .get("partial_json")
                    .and_then(|p| p.as_str())
                    .unwrap_or_default();
                if partial.is_empty() {
                    return Vec::new();
                }
                match self.block_tool_indexes.get(&u64_field(event, "index")) {
                    Some(&tool_index) => vec![StreamEvent::ToolCallChunk {
                        index: tool_index,
                        id: None,
                        name: None,
                        args_delta: Some(partial.to_string()),
                    }],
                    None => Vec::new(),
                }
            }
            // signature_delta / 未知类型静默忽略
            _ => Vec::new(),
        }
    }

    /// message_delta：记录 stop_reason（宽松映射），覆盖式更新 output_tokens
    fn record_message_delta(&mut self, event: &serde_json::Value) {
        if let Some(reason) = event["delta"].get("stop_reason").and_then(|r| r.as_str()) {
            self.finish_reason = match reason {
                "end_turn" => FinishReason::Stop,
                "tool_use" => FinishReason::ToolCalls,
                "max_tokens" => FinishReason::Length,
                _ => FinishReason::Stop,
            };
        }
        let output = u64_field(&event["usage"], "output_tokens");
        if output > 0 {
            self.output_tokens = output as u32;
        }
    }

    /// message_stop：三桶归一后的 Done 事件
    ///
    /// prompt_tokens = 三桶 saturating 求和；prompt_cached_tokens 取缓存读桶；
    /// prompt_cache_creation_tokens 留痕缓存写桶；total = 输入合计 + 输出。
    fn done_event(&self) -> StreamEvent {
        let input_total = self
            .input_tokens
            .saturating_add(self.cache_read_input_tokens)
            .saturating_add(self.cache_creation_input_tokens);
        let usage = StreamUsage {
            prompt_tokens: input_total,
            completion_tokens: self.output_tokens,
            total_tokens: input_total.saturating_add(self.output_tokens),
            completion_reasoning_tokens: None,
            prompt_cached_tokens: Some(self.cache_read_input_tokens),
            prompt_cache_creation_tokens: Some(self.cache_creation_input_tokens),
        };
        StreamEvent::Done {
            usage,
            finish_reason: self.finish_reason.clone(),
        }
    }
}

/// 提取 SSE 行的 data 载荷：空行 / 注释行（`:` 开头）/ 非 `data:` 行返回 None
fn data_payload(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return None;
    }
    line.strip_prefix("data:").map(str::trim)
}

/// u64 数值字段提取，缺失或非数值时为 0
fn u64_field(value: &serde_json::Value, key: &str) -> u64 {
    value.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// error 事件转流错误：协议层异常无 HTTP 状态码，message 取错误体内容
fn error_to_stream_error(event: &serde_json::Value) -> StreamError {
    let error = event.get("error").filter(|e| !e.is_null()).unwrap_or(event);
    let message = error
        .get("message")
        .and_then(|m| m.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string());
    StreamError::ApiError {
        status: None,
        message,
    }
}

impl Default for AnthropicStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 逐行喂入并聚合全部产出事件
    fn feed_all(lines: &[&str]) -> Result<Vec<StreamEvent>, StreamError> {
        let mut decoder = AnthropicStreamDecoder::new();
        let mut events = Vec::new();
        for line in lines {
            events.extend(decoder.feed_line(line)?);
        }
        Ok(events)
    }

    /// 完整事件序列（文本 + 工具调用 + usage）逐项断言
    #[test]
    fn full_event_sequence_produces_unified_events() {
        let events = feed_all(&[
            r#"event: message_start"#,
            r#"data: {"type":"message_start","message":{"id":"msg_01","usage":{"input_tokens":10,"cache_read_input_tokens":100,"cache_creation_input_tokens":20}}}"#,
            "",
            ": keep-alive",
            r#"event: ping"#,
            r#"data: {"type":"ping"}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你好"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01","name":"get_weather","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"北京\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}"#,
            r#"data: {"type":"message_stop"}"#,
        ])
        .unwrap();

        assert_eq!(events.len(), 5, "事件序列：{events:?}");
        assert!(matches!(&events[0], StreamEvent::TextDelta { content } if content == "你好"));
        // tool_use 首达：id + name 增量，工具序号 0（text block 不占工具序号）
        assert!(
            matches!(&events[1], StreamEvent::ToolCallChunk { index, id, name, args_delta }
                if *index == 0
                && id.as_deref() == Some("toolu_01")
                && name.as_deref() == Some("get_weather")
                && args_delta.is_none())
        );
        assert!(
            matches!(&events[2], StreamEvent::ToolCallChunk { index, args_delta, .. }
                if *index == 0 && args_delta.as_deref() == Some("{\"city\":"))
        );
        assert!(
            matches!(&events[3], StreamEvent::ToolCallChunk { index, args_delta, .. }
                if *index == 0 && args_delta.as_deref() == Some("\"北京\"}"))
        );
        match &events[4] {
            StreamEvent::Done {
                usage,
                finish_reason,
            } => {
                assert_eq!(usage.prompt_tokens, 130, "三桶求和 10+100+20");
                assert_eq!(usage.completion_tokens, 42);
                assert_eq!(usage.total_tokens, 172);
                assert_eq!(usage.prompt_cached_tokens, Some(100));
                assert_eq!(usage.prompt_cache_creation_tokens, Some(20));
                assert_eq!(*finish_reason, FinishReason::ToolCalls);
            }
            _ => panic!("期望 Done 事件"),
        }
    }

    /// 多工具各自独立序号互不串扰；thinking / text block 不占工具序号
    #[test]
    fn multiple_tools_keep_independent_indexes() {
        let events = feed_all(&[
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":5}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"内心独白"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig=="}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            // 工具 A：消息级 block 序号 1 → 工具序号 0
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_a","name":"read_file","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            // 中间夹一个 text block：不占工具序号
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"两个工具之间"}}"#,
            r#"data: {"type":"content_block_stop","index":2}"#,
            // 工具 B：消息级 block 序号 3 → 工具序号 1
            r#"data: {"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_b","name":"bash","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"ls\"}"}}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
            r#"data: {"type":"message_stop"}"#,
        ])
        .unwrap();

        // thinking_delta 转思考增量，signature_delta 静默忽略
        assert_eq!(events.len(), 7, "事件序列：{events:?}");
        assert!(
            matches!(&events[0], StreamEvent::ReasoningDelta { content } if content == "内心独白")
        );
        assert!(
            matches!(&events[1], StreamEvent::ToolCallChunk { index, id, name, args_delta }
                if *index == 0
                && id.as_deref() == Some("toolu_a")
                && name.as_deref() == Some("read_file")
                && args_delta.is_none())
        );
        assert!(
            matches!(&events[2], StreamEvent::ToolCallChunk { index, args_delta, .. }
                if *index == 0 && args_delta.as_deref() == Some("{\"path\":"))
        );
        assert!(
            matches!(&events[3], StreamEvent::TextDelta { content } if content == "两个工具之间")
        );
        assert!(
            matches!(&events[4], StreamEvent::ToolCallChunk { index, id, name, .. }
                if *index == 1
                && id.as_deref() == Some("toolu_b")
                && name.as_deref() == Some("bash"))
        );
        assert!(
            matches!(&events[5], StreamEvent::ToolCallChunk { index, args_delta, .. }
                if *index == 1 && args_delta.as_deref() == Some("{\"cmd\":\"ls\"}"))
        );
        assert!(matches!(&events[6], StreamEvent::Done { .. }));
    }

    /// thinking_delta 逐段转 ReasoningDelta；signature_delta 混入不报错不产事件
    #[test]
    fn thinking_delta_maps_to_reasoning_deltas() {
        let events = feed_all(&[
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"第一段"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig=="}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"第二段"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
        ])
        .unwrap();

        // 两个非空思考段各产一个 ReasoningDelta，signature_delta 与空文本无产出
        assert_eq!(events.len(), 2, "事件序列：{events:?}");
        assert!(
            matches!(&events[0], StreamEvent::ReasoningDelta { content } if content == "第一段")
        );
        assert!(
            matches!(&events[1], StreamEvent::ReasoningDelta { content } if content == "第二段")
        );
    }

    /// 纯缓存命中场景：input_tokens 仅为断点后残余，三桶求和归一
    #[test]
    fn usage_normalization_pure_cache_hit() {
        let events = feed_all(&[
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":4,"cache_read_input_tokens":152736,"cache_creation_input_tokens":0}}}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":27}}"#,
            r#"data: {"type":"message_stop"}"#,
        ])
        .unwrap();

        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Done { usage, .. } => {
                assert_eq!(usage.prompt_tokens, 152740, "三桶求和 4+152736+0");
                assert_eq!(usage.prompt_cached_tokens, Some(152736));
                assert_eq!(usage.prompt_cache_creation_tokens, Some(0));
                assert_eq!(usage.completion_tokens, 27);
                assert_eq!(usage.total_tokens, 152767);
            }
            _ => panic!("期望 Done 事件"),
        }
    }

    /// stop_reason 四类映射：end_turn→Stop、tool_use→ToolCalls、max_tokens→Length、
    /// 未知与缺省→Stop
    #[test]
    fn stop_reason_loose_mapping() {
        let cases = [
            ("end_turn", FinishReason::Stop),
            ("tool_use", FinishReason::ToolCalls),
            ("max_tokens", FinishReason::Length),
            ("stop_sequence", FinishReason::Stop),
        ];
        for (reason, expected) in cases {
            let delta_line = format!(
                r#"data: {{"type":"message_delta","delta":{{"stop_reason":"{reason}"}},"usage":{{"output_tokens":1}}}}"#
            );
            let events = feed_all(&[
                r#"data: {"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
                delta_line.as_str(),
                r#"data: {"type":"message_stop"}"#,
            ])
            .unwrap();
            assert!(
                matches!(&events[0], StreamEvent::Done { finish_reason, .. } if *finish_reason == expected),
                "stop_reason={reason} 应映射为 {expected:?}，实际 {events:?}"
            );
        }

        // 缺省：无 message_delta 直接收尾 → Stop
        let events = feed_all(&[
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
            r#"data: {"type":"message_stop"}"#,
        ])
        .unwrap();
        assert!(
            matches!(&events[0], StreamEvent::Done { finish_reason, .. } if *finish_reason == FinishReason::Stop)
        );
    }

    /// 流内 error 事件转协议层异常（无 HTTP 状态码）
    #[test]
    fn error_event_converts_to_api_error_without_status() {
        let mut decoder = AnthropicStreamDecoder::new();
        let result = decoder.feed_line(
            r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        match result {
            Err(StreamError::ApiError { status, message }) => {
                assert_eq!(status, None);
                assert_eq!(message, "Overloaded");
            }
            other => panic!("期望 ApiError，实际 {other:?}"),
        }
    }

    /// 非 data 行 / 空行 / 注释行 / JSON 解析失败的行全部跳过不报错
    #[test]
    fn non_data_and_invalid_json_lines_skipped() {
        let events = feed_all(&[
            "",
            ": comment",
            "event: message_start",
            "data: [DONE]",
            "data: {invalid}",
            "data: not json",
        ])
        .unwrap();
        assert!(events.is_empty());
    }
}
