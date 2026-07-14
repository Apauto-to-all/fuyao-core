//! 消息/请求构造器
//!
//! 从 ReAct 循环抽出的纯构造逻辑：
//! - [`build_chat_request`]：从 session.messages 凑 ChatRequest
//! - [`build_model_and_options`]：从 MessageParams 解析 model + StreamOptions
//! - 各类 Assistant Message / Payload 构造器

use crate::stream::StreamResult;
use crate::tool_registry::ToolRegistry;
use fuyao_api::message::output::{AssistantPayload, ToolCallPayload};
use fuyao_api::{Message, MessageParams, Session};
use fuyao_provider::{ChatMessage, ChatRequest, StreamOptions};

/// 从 session 的内存历史凑 ChatRequest
///
/// 系统提示词单独填 request.system（不进 messages 数组），
/// messages 只装 user/assistant/tool 对话历史。
pub(crate) fn build_chat_request(session: &Session) -> ChatRequest {
    let messages = session
        .messages
        .iter()
        .map(|m| ChatMessage {
            role: m.role.clone(),
            content: m.content.clone(),
            reasoning: m.reasoning.clone(),
            tool_calls: m.tool_calls.as_ref().and_then(|tc| tc.as_array().cloned()),
            tool_call_id: m.tool_call_id.clone(),
            tool_name: m.tool_name.clone(),
        })
        .collect();

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

/// 从流式结果构建 Assistant Message（无工具调用，最终回复）
pub(crate) fn build_assistant_message(result: &StreamResult, model_id: Option<&str>) -> Message {
    let mut msg = Message::assistant(if result.text.is_empty() {
        None
    } else {
        Some(result.text.clone())
    });
    if !result.reasoning.is_empty() {
        msg.reasoning = Some(result.reasoning.clone());
    }
    msg.model_id = model_id.map(|s| s.to_string());
    msg.finish_reason = Some("stop".to_string());
    msg
}

/// 从流式结果构建含 tool_calls 的 Assistant Message（用于进内存历史）
///
/// tool_calls 转成 OpenAI 格式 JSON：[{id, type:"function", function:{name, arguments}}]
pub(crate) fn build_assistant_message_with_tool_calls(
    result: &StreamResult,
    model_id: Option<&str>,
) -> Message {
    let tool_calls_json: Vec<serde_json::Value> = result
        .tool_calls
        .iter()
        .map(|tc| {
            serde_json::json!({
                "id": tc.id,
                "type": "function",
                "function": {
                    "name": tc.name,
                    "arguments": tc.arguments,
                }
            })
        })
        .collect();

    let mut msg = Message::assistant(if result.text.is_empty() {
        None
    } else {
        Some(result.text.clone())
    });
    if !result.reasoning.is_empty() {
        msg.reasoning = Some(result.reasoning.clone());
    }
    msg.tool_calls = Some(serde_json::Value::Array(tool_calls_json));
    msg.model_id = model_id.map(|s| s.to_string());
    msg.finish_reason = Some("tool_calls".to_string());
    msg
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
        completion_tokens: 0,
        prompt_tokens: 0,
        total_tokens: 0,
        reasoning_tokens: 0,
        cached_tokens: 0,
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
        completion_tokens: 0,
        prompt_tokens: 0,
        total_tokens: 0,
        reasoning_tokens: 0,
        cached_tokens: 0,
    }
}
