//! 工具调用事件
//!
//! 当 LLM 决定调用工具时，推送此事件通知前端。
//!
//! `ToolCallPayload` 同时复用于 `AssistantPayload.tool_calls` 数组元素（裸 payload，无 envelope），
//! 见方案第 4.5 节"子 payload 复用清单"。

use crate::message::EventBase;

/// 工具调用事件 envelope
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCallMessage {
    /// 事件元信息（id/timestamp）
    pub base: EventBase,
    /// 工具调用载荷
    pub payload: ToolCallPayload,
}

/// 工具调用载荷（无 base，跨场景复用）
///
/// ① 独立 `ToolCallMessage.payload`（base 由 envelope 提供）
/// ② `AssistantPayload.tool_calls` 数组元素（裸 payload，无 envelope）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCallPayload {
    /// 工具调用 ID
    pub tool_call_id: String,
    /// 工具名称
    pub tool_name: String,
    /// 工具参数
    pub tool_args: serde_json::Value,
}

// ===== OpenAI 嵌套 tool_calls schema 的构造与解析 =====
//
// Message.tool_calls 落库与 ChatMessage 透传时都用 OpenAI 嵌套形态：
//   { "id": "...", "type": "function", "function": { "name": "...", "arguments": "..." } }
// schema 的字段名 / 嵌套结构是单一真源，集中在此处构造与解析，
// 调用点不再各自硬编码字段名——改协议字段只改这里。

/// 构造单条 OpenAI 嵌套 tool_call JSON
///
/// `arguments` 原样塞入（OpenAI 协议中它是 JSON 字符串）。
/// 调用点持 `ToolCallData` 或 `ToolCallPayload` 都能直接传字段，零类型耦合。
pub fn build_nested_tool_call(id: &str, name: &str, arguments: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": arguments}
    })
}

/// 从 OpenAI 嵌套 tool_calls JSON 数组提取 `(id, name)` 对
///
/// 用于配对检查（缺结果的 tool_call 补 error result）等只需标识与名称的场景。
/// 非数组 / 元素缺 `id` 或 `function.name` 时跳过该元素（容错），不整体失败。
pub fn extract_id_name_pairs(tool_calls: &serde_json::Value) -> Vec<(String, String)> {
    let Some(arr) = tool_calls.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|tc| {
            let id = tc.get("id").and_then(|v| v.as_str())?;
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())?;
            Some((id.to_string(), name.to_string()))
        })
        .collect()
}

/// 从单条 OpenAI 嵌套 tool_call JSON 提取扁平 [`ToolCallPayload`]
///
/// 字段映射：`id → tool_call_id`、`function.name → tool_name`、
/// `function.arguments`(JSON 字符串) → `tool_args`(解析后的 `Value`)。
/// `arguments` 解析失败时兜底成原字符串 `Value`，保证信息不丢。
///
/// 缺字段返回 `None`（容错：单条工具调用损坏不应阻断整段历史）。
pub fn parse_nested_tool_call(v: &serde_json::Value) -> Option<ToolCallPayload> {
    let id = v.get("id").and_then(|i| i.as_str())?;
    let function = v.get("function")?;
    let name = function.get("name").and_then(|n| n.as_str())?;
    // arguments 是 JSON 字符串：解析失败兜底成原字符串 Value，信息不丢
    let tool_args = function
        .get("arguments")
        .and_then(|a| a.as_str())
        .map(|s| serde_json::from_str(s).unwrap_or_else(|_| serde_json::Value::String(s.into())))
        .unwrap_or(serde_json::Value::Null);

    Some(ToolCallPayload {
        tool_call_id: id.into(),
        tool_name: name.into(),
        tool_args,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    #[test]
    fn tool_call_holds_fields() {
        let msg = ToolCallMessage {
            base: EventBase::default(),
            payload: ToolCallPayload {
                tool_call_id: "call_1".into(),
                tool_name: "get_weather".into(),
                tool_args: serde_json::json!({"city": "北京"}),
            },
        };
        assert_eq!(msg.payload.tool_call_id, "call_1");
        assert_eq!(msg.payload.tool_name, "get_weather");
        assert_eq!(msg.payload.tool_args["city"], "北京");
    }

    #[test]
    fn tool_call_args_empty_object() {
        let msg = ToolCallMessage {
            base: EventBase::default(),
            payload: ToolCallPayload {
                tool_call_id: "call_2".into(),
                tool_name: "noop".into(),
                tool_args: serde_json::json!({}),
            },
        };
        assert_eq!(msg.payload.tool_args, serde_json::json!({}));
    }

    #[test]
    fn tool_call_clone_works() {
        let msg = ToolCallMessage {
            base: EventBase::default(),
            payload: ToolCallPayload {
                tool_call_id: "call_1".into(),
                tool_name: "search".into(),
                tool_args: serde_json::json!({"q": "rust"}),
            },
        };
        let cloned = msg.clone();
        assert_eq!(msg.payload.tool_call_id, cloned.payload.tool_call_id);
        assert_eq!(msg.payload.tool_name, cloned.payload.tool_name);
        assert_eq!(msg.payload.tool_args, cloned.payload.tool_args);
    }

    #[test]
    fn tool_call_timestamp_is_set() {
        let msg = ToolCallMessage {
            base: EventBase::default(),
            payload: ToolCallPayload {
                tool_call_id: "call_1".into(),
                tool_name: "test".into(),
                tool_args: serde_json::json!({}),
            },
        };
        assert!(msg.base.timestamp > 0.0);
    }

    #[test]
    fn tool_call_payload_reuse_bare() {
        // 验证裸 payload 可独立构造（供 AssistantPayload.tool_calls 子元素复用）
        let payload = ToolCallPayload {
            tool_call_id: "call_1".into(),
            tool_name: "search".into(),
            tool_args: serde_json::json!({"q": "rust"}),
        };
        assert_eq!(payload.tool_name, "search");
    }

    // ===== build_nested_tool_call =====

    #[test]
    fn build_nested_produces_openai_schema() {
        let v = build_nested_tool_call("call_1", "search", r#"{"q":"rust"}"#);
        assert_eq!(v["id"], "call_1");
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "search");
        // arguments 原样塞入（JSON 字符串，不被二次解析）
        assert_eq!(v["function"]["arguments"], r#"{"q":"rust"}"#);
    }

    #[test]
    fn build_nested_empty_arguments() {
        let v = build_nested_tool_call("call_2", "noop", "{}");
        assert_eq!(v["function"]["arguments"], "{}");
    }

    // ===== extract_id_name_pairs =====

    #[test]
    fn extract_pairs_normal_array() {
        let arr = serde_json::json!([
            {"id": "c1", "type": "function", "function": {"name": "search", "arguments": "{}"}},
            {"id": "c2", "type": "function", "function": {"name": "read", "arguments": "{}"}}
        ]);
        let pairs = extract_id_name_pairs(&arr);
        assert_eq!(
            pairs,
            vec![("c1".into(), "search".into()), ("c2".into(), "read".into())]
        );
    }

    #[test]
    fn extract_pairs_skips_missing_id() {
        let arr = serde_json::json!([
            {"id": "c1", "type": "function", "function": {"name": "search", "arguments": "{}"}},
            {"type": "function", "function": {"name": "no_id", "arguments": "{}"}}
        ]);
        let pairs = extract_id_name_pairs(&arr);
        assert_eq!(pairs, vec![("c1".into(), "search".into())]);
    }

    #[test]
    fn extract_pairs_skips_missing_function_name() {
        let arr = serde_json::json!([
            {"id": "c1", "function": {"arguments": "{}"}},
            {"id": "c2", "function": {"name": "ok", "arguments": "{}"}}
        ]);
        let pairs = extract_id_name_pairs(&arr);
        assert_eq!(pairs, vec![("c2".into(), "ok".into())]);
    }

    #[test]
    fn extract_pairs_non_array_returns_empty() {
        let pairs = extract_id_name_pairs(&serde_json::json!({"id": "c1"}));
        assert!(pairs.is_empty());
    }

    #[test]
    fn extract_pairs_empty_array() {
        let pairs = extract_id_name_pairs(&serde_json::json!([]));
        assert!(pairs.is_empty());
    }

    // ===== parse_nested_tool_call =====

    #[test]
    fn parse_nested_normal() {
        let v = serde_json::json!({
            "id": "call_1", "type": "function",
            "function": {"name": "search", "arguments": "{\"q\":\"rust\"}"}
        });
        let p = parse_nested_tool_call(&v).expect("正常 schema 应解析成功");
        assert_eq!(p.tool_call_id, "call_1");
        assert_eq!(p.tool_name, "search");
        assert_eq!(p.tool_args, serde_json::json!({"q": "rust"}));
    }

    #[test]
    fn parse_nested_arguments_not_json_fallback_to_string() {
        // arguments 非 JSON：兜底成原字符串 Value，信息不丢
        let v = serde_json::json!({
            "id": "call_1", "type": "function",
            "function": {"name": "bad", "arguments": "not-json"}
        });
        let p = parse_nested_tool_call(&v).expect("缺可解析 arguments 不应整体失败");
        assert_eq!(p.tool_args, serde_json::Value::String("not-json".into()));
    }

    #[test]
    fn parse_nested_missing_id_returns_none() {
        let v =
            serde_json::json!({"type": "function", "function": {"name": "x", "arguments": "{}"}});
        assert!(parse_nested_tool_call(&v).is_none());
    }

    #[test]
    fn parse_nested_missing_function_returns_none() {
        let v = serde_json::json!({"id": "call_1", "type": "function"});
        assert!(parse_nested_tool_call(&v).is_none());
    }

    // ===== 往返一致性 =====

    #[test]
    fn build_then_parse_roundtrip() {
        // build_nested_tool_call → parse_nested_tool_call 回到等价 payload
        let v = build_nested_tool_call("call_1", "search", r#"{"q":"rust"}"#);
        let p = parse_nested_tool_call(&v).expect("构造出的 schema 应能解析回来");
        assert_eq!(p.tool_call_id, "call_1");
        assert_eq!(p.tool_name, "search");
        assert_eq!(p.tool_args, serde_json::json!({"q": "rust"}));
    }
}
