//! 请求体编码（纯函数，不依赖 HTTP）
//!
//! 把内部 [`ChatRequest`] 翻译为 OpenAI 兼容的 `/chat/completions` 请求体 JSON。
//! 与传输层解耦：输入是领域消息结构，输出是 `serde_json::Value`，可独立单测。

use crate::provider::{ChatRequest, StreamOptions};
use fuyao_api::{ImageContent, MessageRole, ThinkingType};

/// OpenAI 协议支持的图片 MIME 白名单（仅图像：PNG / JPEG / WEBP / 非动画 GIF）
const SUPPORTED_IMAGE_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// 单图解码后字节上限（OpenAI 协议约束；base64 长度 /4*3 估算解码字节数）
pub(crate) const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// 校验并过滤图片：MIME 白名单 + 协议字节上限，不合规的丢弃并告警
///
/// 校验失败只丢单张图、不中断整条消息——图像是辅助信息，文本对话照常。
fn filter_valid_images(images: &[ImageContent]) -> Vec<&ImageContent> {
    images
        .iter()
        .filter(|img| {
            if !SUPPORTED_IMAGE_MIMES.contains(&img.mime_type.as_str()) {
                tracing::warn!(mime_type = %img.mime_type, "图片 MIME 不在协议白名单，已丢弃");
                return false;
            }
            if img.data.len() / 4 * 3 > MAX_IMAGE_BYTES {
                tracing::warn!(
                    mime_type = %img.mime_type,
                    base64_len = img.data.len(),
                    "图片超过 20MB 协议上限，已丢弃"
                );
                return false;
            }
            true
        })
        .collect()
}

/// 将内部 ChatRequest 转换为 OpenAI API 请求体
///
/// 纯函数：不触碰 `self` / HTTP，仅做领域消息 → wire JSON 的翻译。
/// 流式 / 非流式两路径共用，由 `stream` 形参区分是否注入 `stream`/`stream_options`。
pub(crate) fn build_request_body(
    request: ChatRequest,
    model: &str,
    options: &StreamOptions,
    stream: bool,
) -> serde_json::Value {
    let mut messages = Vec::new();

    // 系统消息
    if let Some(system) = request.system {
        messages.push(serde_json::json!({
            "role": "system",
            "content": system,
        }));
    }

    // 对话消息
    for msg in request.messages {
        let mut msg_value = serde_json::json!({
            "role": msg.role.as_str(),
        });

        // 内容：带图 user 消息转 parts 数组（text + image_url），纯文本保持字符串（零回归）
        let valid_images = if matches!(msg.role, MessageRole::User) {
            filter_valid_images(&msg.images)
        } else {
            if !msg.images.is_empty() {
                tracing::warn!(role = %msg.role.as_str(), "非 user 消息携带图片，已忽略");
            }
            vec![]
        };

        if valid_images.is_empty() {
            // 无图（含图片全部被过滤）：保持原纯字符串形态
            if let Some(content) = &msg.content {
                msg_value["content"] = serde_json::Value::String(content.clone());
            }
        } else {
            // 有图：content 升级为 parts 数组，图片以 data URL 内联
            let mut parts: Vec<serde_json::Value> = Vec::new();
            if let Some(text) = &msg.content
                && !text.is_empty()
            {
                parts.push(serde_json::json!({ "type": "text", "text": text }));
            }
            for img in valid_images {
                parts.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{};base64,{}", img.mime_type, img.data),
                    }
                }));
            }
            msg_value["content"] = serde_json::Value::Array(parts);
        }

        // 思考内容（部分供应商需要在历史消息中传递）
        if let Some(reasoning) = &msg.reasoning {
            msg_value["reasoning_content"] = serde_json::Value::String(reasoning.clone());
        }

        // 工具调用：typed 字段直接构造 OpenAI 嵌套 wire 形态（id / type / function 三层）
        if let Some(tool_calls) = &msg.tool_calls {
            msg_value["tool_calls"] = serde_json::Value::Array(
                tool_calls
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
                    .collect(),
            );
        }

        // 工具调用 ID
        if let Some(tool_call_id) = &msg.tool_call_id {
            msg_value["tool_call_id"] = serde_json::Value::String(tool_call_id.clone());
        }

        messages.push(msg_value);
    }

    let mut body = serde_json::json!({
        "model": model,
        "messages": messages,
    });

    // 流式
    if stream {
        body["stream"] = serde_json::Value::Bool(true);
        body["stream_options"] = serde_json::json!({
            "include_usage": true,
        });
    }

    // 工具（tool_choice 不发送，走服务器默认的 auto 语义）
    if let Some(tools) = &options.tools
        && !tools.is_empty()
    {
        // 中立工具定义逐个包壳为协议 wire 形态：
        // {"type":"function","function":{name, description, parameters}}
        let wire_tools: Vec<serde_json::Value> = tools
            .iter()
            .filter_map(|def| serde_json::to_value(def).ok())
            .map(|function| serde_json::json!({ "type": "function", "function": function }))
            .collect();
        body["tools"] = serde_json::Value::Array(wire_tools);
    }

    // 思考字段独立注入：thinking_type 与 reasoning_effort 是两个正交字段，
    // 各自为 Some 时各自发送，互不压制。配置了就必须发——禁止因 thinking_type=Disabled
    // 而压掉 reasoning_effort，两者由服务器各自解释，fuyao 不替服务器做语义裁剪。
    if let Some(t) = &options.thinking_type {
        // thinking.type 是 OpenAI 协议约定的小写字面量，在请求构造层固化，
        // 与 ThinkingType 枚举的序列化形态解耦——枚举形态只服务 IPC
        let thinking_type_str = match t {
            ThinkingType::Enabled => "enabled",
            ThinkingType::Disabled => "disabled",
        };
        body["thinking"] = serde_json::json!({ "type": thinking_type_str });
    }
    if let Some(e) = &options.reasoning_effort {
        body["reasoning_effort"] = serde_json::Value::String(e.clone());
    }

    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatMessage, ChatRequest, StreamOptions as ProviderStreamOptions};
    use fuyao_api::{ImageContent, MessageRole, ThinkingType, ToolCallData};

    #[test]
    fn build_request_body_image_message_uses_parts_array() {
        // 带图 user 消息：content 升级为 parts 数组（text + image_url data URL）
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("看图".to_string()),
                images: vec![ImageContent {
                    mime_type: "image/png".into(),
                    data: "aGVsbG8=".into(),
                }],
                ..Default::default()
            }],
            system: None,
        };
        let body = build_request_body(
            request,
            "qwen3.6-plus",
            &ProviderStreamOptions::default(),
            false,
        );

        let content = &body["messages"][0]["content"];
        assert!(content.is_array(), "带图消息 content 应为数组");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "看图");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            "data:image/png;base64,aGVsbG8="
        );
    }

    #[test]
    fn build_request_body_image_message_without_text_omits_text_part() {
        // content 为空时只发图片 part
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: None,
                images: vec![ImageContent {
                    mime_type: "image/webp".into(),
                    data: "d2VicA==".into(),
                }],
                ..Default::default()
            }],
            system: None,
        };
        let body = build_request_body(
            request,
            "qwen3.6-plus",
            &ProviderStreamOptions::default(),
            false,
        );

        let content = &body["messages"][0]["content"];
        assert_eq!(content.as_array().map(Vec::len), Some(1));
        assert_eq!(content[0]["type"], "image_url");
    }

    #[test]
    fn build_request_body_drops_unsupported_mime_image() {
        // 非法 MIME 图片被丢弃：消息回退为纯文本字符串形态
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("文本".to_string()),
                images: vec![
                    ImageContent {
                        mime_type: "application/pdf".into(),
                        data: "x".into(),
                    },
                    ImageContent {
                        mime_type: "image/png".into(),
                        data: "aGVsbG8=".into(),
                    },
                ],
                ..Default::default()
            }],
            system: None,
        };
        let body = build_request_body(
            request,
            "qwen3.6-plus",
            &ProviderStreamOptions::default(),
            false,
        );

        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content.as_array().map(Vec::len), Some(2), "pdf 图应被丢弃");
    }

    #[test]
    fn build_request_body_drops_oversized_image() {
        // 超过 20MB 协议上限的图片被丢弃（base64 长度 = 解码字节 × 4/3）
        let oversized = "A".repeat(MAX_IMAGE_BYTES / 3 * 4 + 100);
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("大图".to_string()),
                images: vec![ImageContent {
                    mime_type: "image/png".into(),
                    data: oversized,
                }],
                ..Default::default()
            }],
            system: None,
        };
        let body = build_request_body(
            request,
            "qwen3.6-plus",
            &ProviderStreamOptions::default(),
            false,
        );

        let content = &body["messages"][0]["content"];
        assert_eq!(content, "大图", "超限图被丢弃后应回退为纯字符串");
    }

    #[test]
    fn build_request_body_ignores_images_on_non_user_messages() {
        // 防御：非 user 角色携带图片时忽略（协议侧 tool/assistant content 只认字符串）
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::Assistant,
                content: Some("回复".to_string()),
                images: vec![ImageContent {
                    mime_type: "image/png".into(),
                    data: "aGVsbG8=".into(),
                }],
                ..Default::default()
            }],
            system: None,
        };
        let body = build_request_body(
            request,
            "qwen3.6-plus",
            &ProviderStreamOptions::default(),
            false,
        );

        assert_eq!(body["messages"][0]["content"], "回复");
    }

    #[test]
    fn build_request_body_includes_model_and_messages() {
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("hello".to_string()),
                ..Default::default()
            }],
            system: None,
        };
        let body = build_request_body(
            request,
            "qwen3.6-plus",
            &ProviderStreamOptions::default(),
            false,
        );

        assert_eq!(body["model"], "qwen3.6-plus");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
        assert!(body.get("stream").is_none());
    }

    #[test]
    fn build_request_body_stream_mode() {
        let body = build_request_body(
            ChatRequest::default(),
            "qwen3.6-plus",
            &ProviderStreamOptions::default(),
            true,
        );

        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn build_request_body_with_system() {
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("hi".to_string()),
                ..Default::default()
            }],
            system: Some("你是助手".to_string()),
        };
        let body = build_request_body(
            request,
            "qwen3.6-plus",
            &ProviderStreamOptions::default(),
            false,
        );

        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "你是助手");
        assert_eq!(body["messages"][1]["role"], "user");
    }

    #[test]
    fn build_request_body_tool_calls_wire_shape() {
        // typed ToolCallData → OpenAI 嵌套 wire 形态：id / type / function.{name,arguments}
        // 层级与字段逐项锁定（协议形状的唯一栖息地在本模块，此处钉死输出形态）
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::Assistant,
                content: None,
                tool_calls: Some(vec![ToolCallData {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    arguments: "{}".to_string(),
                }]),
                ..Default::default()
            }],
            system: None,
        };
        let body = build_request_body(
            request,
            "qwen3.6-plus",
            &ProviderStreamOptions::default(),
            false,
        );

        assert_eq!(
            body["messages"][0]["tool_calls"],
            serde_json::json!([{
                "id": "call_1",
                "type": "function",
                "function": {"name": "bash", "arguments": "{}"}
            }])
        );
        // 纯工具调用消息 content=None，不注入 content 字段
        assert!(body["messages"][0].get("content").is_none());
    }

    #[test]
    fn build_request_body_with_tools() {
        let request = ChatRequest::default();
        let options = ProviderStreamOptions {
            tools: Some(vec![fuyao_api::ToolDefinition::new("bash", "执行命令")]),
            ..Default::default()
        };
        let body = build_request_body(request, "qwen3.6-plus", &options, false);

        assert!(body["tools"].is_array());
        // tool_choice 不发送（走服务器默认的 auto 语义）
        assert!(body.get("tool_choice").is_none());
        // temperature 不在请求参数内（走服务器默认）
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn build_request_body_no_thinking_when_both_none() {
        // 思考模型默认（两参数都 None）：请求体不含思考字段
        let body = build_request_body(
            ChatRequest::default(),
            "deepseek-v4-flash",
            &ProviderStreamOptions::default(),
            false,
        );
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn build_request_body_thinking_enabled_only() {
        // 开思考但不设强度：仅发 thinking:enabled
        let options = ProviderStreamOptions {
            thinking_type: Some(ThinkingType::Enabled),
            ..Default::default()
        };
        let body = build_request_body(ChatRequest::default(), "deepseek-v4-flash", &options, false);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn build_request_body_thinking_enabled_with_effort() {
        // 开思考 + 指定强度：两者都发
        let options = ProviderStreamOptions {
            thinking_type: Some(ThinkingType::Enabled),
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        let body = build_request_body(ChatRequest::default(), "deepseek-v4-flash", &options, false);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn build_request_body_accepts_arbitrary_effort_string() {
        // 自定义档位名（如 "big" 不在常见枚举内）原样透传，fuyao 不校验
        let options = ProviderStreamOptions {
            thinking_type: Some(ThinkingType::Enabled),
            reasoning_effort: Some("big".to_string()),
            ..Default::default()
        };
        let body = build_request_body(ChatRequest::default(), "weird-model", &options, false);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "big");
    }

    #[test]
    fn build_request_body_thinking_disabled_still_sends_effort() {
        // 两字段独立：thinking_type=Disabled 不压制 reasoning_effort，配了就发
        let options = ProviderStreamOptions {
            thinking_type: Some(ThinkingType::Disabled),
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        let body = build_request_body(ChatRequest::default(), "deepseek-v4-flash", &options, false);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert_eq!(body["reasoning_effort"], "high");
    }
}
