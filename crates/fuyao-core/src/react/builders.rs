//! 消息/请求构造器
//!
//! 从 ReAct 循环抽出的纯构造逻辑：
//! - [`build_chat_request`]：从 session.messages 凑 ChatRequest
//! - [`build_model_and_options`]：从 MessageParams 解析 model + StreamOptions
//! - 各类 Assistant Payload 构造器（事件用）
//! - 工具调用拦截回灌用的双向转换函数
//! - [`fill_assistant_message_usage_and_cost`]：填 assistant Message 的 token + cost 字段
//!
//! 注：Message 主体构造由 turn.rs 的 emit_to_history 闭包内联完成（因每种事件的字段映射不同，
//! 集中成 trait 反而过度抽象）。本模块只留 payload 构造、请求构造与 token/cost 填充。

use crate::stream::StreamResult;
use crate::tool_registry::ToolRegistry;
use fuyao_api::message::output::{AssistantPayload, ToolCallMessage, ToolCallPayload};
use fuyao_api::message::{EventBase, OutputEvent};
use fuyao_api::{MessageParams, Session};
use fuyao_provider::{ChatMessage, ChatRequest, StreamOptions, ToolCallData};

/// 从 session 的内存历史凑 ChatRequest
///
/// 系统提示词单独填 request.system（不进 messages 数组），
/// messages 只装 user/assistant/tool 对话历史。
///
/// **配对兜底**：OpenAI/Anthropic 协议要求每个 assistant 的 tool_call 都有对应的
/// tool 结果消息。被拦截 Block、中断的工具调用不会有结果——这里在拼消息时
/// 为缺结果的 tool_call 补一条 error tool_result（content 标记中断）。
pub(crate) fn build_chat_request(session: &Session) -> ChatRequest {
    // 先收集所有已有 tool 结果的 tool_call_id（用于配对检查）
    let mut answered_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for m in &session.messages {
        if m.role == "tool"
            && let Some(id) = &m.tool_call_id
        {
            answered_ids.insert(id.as_str());
        }
    }

    let mut messages = Vec::with_capacity(session.messages.len());
    for m in &session.messages {
        messages.push(ChatMessage {
            role: m.role.clone(),
            content: m.content.clone(),
            reasoning: m.reasoning.clone(),
            tool_calls: m.tool_calls.as_ref().and_then(|tc| tc.as_array().cloned()),
            tool_call_id: m.tool_call_id.clone(),
            tool_name: m.tool_name.clone(),
        });

        // assistant 消息后：为缺结果的 tool_call 补 error tool_result
        if m.role == "assistant"
            && let Some(tool_calls) = m.tool_calls.as_ref().and_then(|tc| tc.as_array())
        {
            for tc in tool_calls {
                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if !id.is_empty() && !answered_ids.contains(id) {
                    // 缺结果：补 error tool_result
                    messages.push(ChatMessage {
                        role: "tool".to_string(),
                        content: Some("[工具执行被拦截或中断]".to_string()),
                        reasoning: None,
                        tool_calls: None,
                        tool_call_id: Some(id.to_string()),
                        tool_name: tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                    });
                }
            }
        }
    }

    ChatRequest {
        messages,
        system: session.system_prompt.clone(),
    }
}

/// 从 MessageParams 解析模型名 + 构建流式选项
///
/// model_id 格式 "provider/model" → 取 '/' 后的 model 部分。
/// 思考控制参数（thinking_type / reasoning_effort）透传给 StreamOptions。
/// 工具定义从 registry 序列化（非空时带 tools 字段）。
// TODO: model_id 为 None 时用配置的默认模型（当前空串，provider 自行处理）
pub(crate) fn build_model_and_options(
    params: &MessageParams,
    tools: &ToolRegistry,
) -> (String, StreamOptions) {
    let model = params
        .model_config
        .model_id
        .as_deref()
        .and_then(|id| id.split('/').nth(1))
        .unwrap_or("")
        .to_string();

    let tool_defs = tools.definitions_json();
    let options = StreamOptions {
        temperature: None,
        tools: if tool_defs.is_empty() {
            None
        } else {
            Some(tool_defs)
        },
        tool_choice: None,
        thinking_type: params.model_config.thinking_type.clone(),
        reasoning_effort: params.model_config.reasoning_effort.clone(),
    };

    (model, options)
}

/// 从流式结果构建 AssistantPayload（无工具调用，最终回复事件）
pub(crate) fn assistant_msg_to_payload(result: &StreamResult) -> AssistantPayload {
    AssistantPayload {
        content: if result.text.is_empty() {
            None
        } else {
            Some(result.text.clone())
        },
        reasoning: if result.reasoning.is_empty() {
            None
        } else {
            Some(result.reasoning.clone())
        },
        tool_calls: None,
        finish_reason: Some("stop".to_string()),
        completion_tokens: result.usage.completion_tokens as i64,
        prompt_tokens: result.usage.prompt_tokens as i64,
        total_tokens: result.usage.total_tokens as i64,
        reasoning_tokens: result.usage.completion_reasoning_tokens.unwrap_or(0) as i64,
        cached_tokens: result.usage.prompt_cached_tokens.unwrap_or(0) as i64,
    }
}

/// 从流式结果构建含 tool_calls 的 AssistantPayload（工具调用事件）
pub(crate) fn assistant_with_tool_calls_to_payload(result: &StreamResult) -> AssistantPayload {
    let tool_call_payloads: Vec<ToolCallPayload> = result
        .tool_calls
        .iter()
        .map(|tc| ToolCallPayload {
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            tool_args: serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null),
        })
        .collect();

    AssistantPayload {
        content: if result.text.is_empty() {
            None
        } else {
            Some(result.text.clone())
        },
        reasoning: if result.reasoning.is_empty() {
            None
        } else {
            Some(result.reasoning.clone())
        },
        tool_calls: Some(tool_call_payloads),
        finish_reason: Some("tool_calls".to_string()),
        completion_tokens: result.usage.completion_tokens as i64,
        prompt_tokens: result.usage.prompt_tokens as i64,
        total_tokens: result.usage.total_tokens as i64,
        reasoning_tokens: result.usage.completion_reasoning_tokens.unwrap_or(0) as i64,
        cached_tokens: result.usage.prompt_cached_tokens.unwrap_or(0) as i64,
    }
}

// ===== 工具调用拦截回灌用的转换函数 =====
//
// 工具调用逐个经 dispatch_intercept 拦截后，需要从拦截后的 OutputEvent::ToolCall
// 提取出执行用的 ToolCallData（参数可能被插件修改），保证「执行 / 存储 / 发送」
// 三者数据一致（都以拦截后的 payload 为准）。
//
// 注：assistant Message 的 token + cost 字段填充归 fuyao-session 的 `fill_message_cost`
// 统一实现（所有费用计算集中在 session 模块，避免散落）。

/// 把单个工具调用数据构造成 ToolCall 输出事件（供逐个拦截用）
pub(crate) fn tool_call_data_to_event(tc: &ToolCallData) -> OutputEvent {
    OutputEvent::ToolCall(ToolCallMessage {
        base: EventBase::default(),
        payload: ToolCallPayload {
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            tool_args: serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null),
        },
    })
}

/// 从拦截后的 ToolCall 事件提取执行用的工具调用数据
///
/// 插件可能修改了 tool_name / tool_args，这里以拦截后的 payload 为准构造 ToolCallData。
/// 参数序列化回 JSON 字符串（execute_tools 内部按字符串解析参数）。
pub(crate) fn tool_call_event_to_data(event: &OutputEvent) -> Option<ToolCallData> {
    if let OutputEvent::ToolCall(msg) = event {
        Some(ToolCallData {
            id: msg.payload.tool_call_id.clone(),
            name: msg.payload.tool_name.clone(),
            arguments: msg.payload.tool_args.to_string(),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::{Message, Session};

    /// 构造带工具调用的 assistant Message
    fn assistant_with_calls(ids: &[&str]) -> Message {
        let tool_calls: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| {
                serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": "echo", "arguments": "{}"}
                })
            })
            .collect();
        let mut msg = Message::assistant(Some("调用工具".to_string()));
        msg.tool_calls = Some(serde_json::Value::Array(tool_calls));
        msg.finish_reason = Some("tool_calls".to_string());
        msg
    }

    #[test]
    fn pairing_fills_missing_tool_results() {
        // assistant 调用 3 个工具，只有 1 个有结果 → 补 2 条 error tool_result
        let mut session = Session::new(None, Some("系统提示词".to_string()));
        session.messages.push(Message::user("问题".to_string()));
        session
            .messages
            .push(assistant_with_calls(&["c1", "c2", "c3"]));
        session
            .messages
            .push(Message::tool_result("c2".into(), "结果2".into()));

        let request = build_chat_request(&session);

        // 应有：user + assistant + 1 真实结果 + 2 补充 error 结果 = 5 条
        let tool_msgs: Vec<_> = request
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .collect();
        assert_eq!(tool_msgs.len(), 3, "应有 3 条 tool 消息（1真实+2补充）");

        // c1 和 c3 被补充
        let supplemented_ids: Vec<_> = tool_msgs
            .iter()
            .filter(|m| m.content.as_deref() == Some("[工具执行被拦截或中断]"))
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        assert_eq!(
            supplemented_ids,
            vec!["c1", "c3"],
            "c1 和 c3 应被补充 error"
        );
    }

    #[test]
    fn pairing_no_op_when_all_answered() {
        // 所有 tool_call 都有结果 → 不补充
        let mut session = Session::new(None, Some("系统提示词".to_string()));
        session.messages.push(assistant_with_calls(&["c1", "c2"]));
        session
            .messages
            .push(Message::tool_result("c1".into(), "结果1".into()));
        session
            .messages
            .push(Message::tool_result("c2".into(), "结果2".into()));

        let request = build_chat_request(&session);
        let tool_count = request.messages.iter().filter(|m| m.role == "tool").count();
        assert_eq!(tool_count, 2, "全部有结果时不补充");
    }

    #[test]
    fn pairing_ignores_assistant_without_tool_calls() {
        // 无工具调用的 assistant 消息不触发补充
        let mut session = Session::new(None, Some("系统提示词".to_string()));
        session.messages.push(Message::user("你好".to_string()));
        session
            .messages
            .push(Message::assistant(Some("你好".to_string())));

        let request = build_chat_request(&session);
        assert_eq!(request.messages.len(), 2, "无工具调用时消息数不变");
    }
}
