//! Anthropic Messages 协议请求体编码（纯函数，不依赖 HTTP）
//!
//! 把内部 [`ChatRequest`] 翻译为 Anthropic `/v1/messages` 请求体 JSON。
//! 与传输层解耦：输入是领域消息结构，输出是 `serde_json::Value`，可独立单测。
//!
//! 协议正确性的两条硬边界由本模块保证：
//! - 产出消息序列永无相邻同角色消息（连续工具结果、工具结果后紧跟的 user
//!   消息都合并进同一条 user 消息，Anthropic 相邻同角色消息直接 400）
//! - 每个历史 tool_use 必有配对 tool_result，孤儿场景合成中文占位 stub

use crate::provider::{ChatMessage, ChatRequest, StreamOptions};
use fuyao_api::{ImageContent, MessageRole, ThinkingType};
use serde::Serialize;
use std::collections::HashSet;

/// Anthropic 协议支持的图片 MIME 白名单（仅图像：PNG / JPEG / WEBP / GIF）
const SUPPORTED_IMAGE_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// 单图解码后字节上限（base64 长度 /4*3 估算解码字节数）
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// max_tokens 必填字段的恒定值
const MAX_TOKENS: u32 = 16384;

/// 孤儿 tool_use 的兜底 tool_result 占位文本（中断 / 恢复 / 压缩边界的历史重放防线）
const ORPHAN_TOOL_RESULT_STUB: &str = "工具结果缺失：该轮次在工具完成前被中断";

/// 有图无文本时的占位文本（content 数组不允许只有 image block）
const IMAGE_PLACEHOLDER_TEXT: &str = "[图片]";

/// ephemeral 缓存断点（prompt cache 标记的唯一形态）
#[derive(Debug, Serialize)]
struct CacheControl {
    /// 断点类型恒为 ephemeral
    #[serde(rename = "type")]
    kind: &'static str,
}

impl CacheControl {
    /// 构造 ephemeral 断点
    fn ephemeral() -> Self {
        Self { kind: "ephemeral" }
    }
}

/// base64 内联图片源
#[derive(Debug, Serialize)]
struct Base64ImageSource {
    /// 源类型恒为 base64 内联
    #[serde(rename = "type")]
    kind: &'static str,
    media_type: String,
    data: String,
}

/// content block 出方向形态（出方向严格 enum，入方向解析见各消费模块）
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ContentBlock {
    /// 文本块（缓存断点可选）
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// base64 内联图片块
    #[serde(rename = "image")]
    Image { source: Base64ImageSource },
    /// assistant 发起的工具调用块
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// user 回传的工具结果块
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

/// 消息 content 出方向双形态：纯文本字符串 | block 数组
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl MessageContent {
    /// 展开为 block 数组：字符串形态升格为单 Text block（空字符串升格为空数组）
    fn into_blocks(self) -> Vec<ContentBlock> {
        match self {
            Self::Text(text) if text.is_empty() => Vec::new(),
            Self::Text(text) => vec![ContentBlock::Text {
                text,
                cache_control: None,
            }],
            Self::Blocks(blocks) => blocks,
        }
    }
}

/// Anthropic messages 数组元素
#[derive(Debug, Serialize)]
struct WireMessage {
    role: &'static str,
    content: MessageContent,
}

/// Anthropic 工具定义 wire 形态
#[derive(Debug, Serialize)]
struct WireTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

/// stream 字段的 skip 判定（false 时不序列化）
fn is_false(value: &bool) -> bool {
    !value
}

/// /v1/messages 请求体顶层形态
///
/// 该协议不存在的概念（thinking / reasoning_effort / tool_choice）不设字段，
/// 「不发送」由结构体缺位直接保证。
#[derive(Debug, Serialize)]
struct MessagesRequestBody {
    model: String,
    messages: Vec<WireMessage>,
    max_tokens: u32,
    /// 恒用 Blocks 形态挂缓存断点
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<ContentBlock>>,
    #[serde(skip_serializing_if = "is_false")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<WireTool>>,
}

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

/// user 消息 content 出方向形态：无图保持字符串，有图升级为 block 数组
fn user_content(msg: &ChatMessage) -> MessageContent {
    let valid_images = filter_valid_images(&msg.images);
    let text = msg.content.clone().unwrap_or_default();
    let Some(first_image) = valid_images.first() else {
        return MessageContent::Text(text);
    };
    // content 数组不允许只有 image：文本为空时补占位文本
    let text = if text.is_empty() {
        IMAGE_PLACEHOLDER_TEXT.to_string()
    } else {
        text
    };
    let mut blocks = vec![ContentBlock::Text {
        text,
        cache_control: None,
    }];
    blocks.push(ContentBlock::Image {
        source: Base64ImageSource {
            kind: "base64",
            media_type: first_image.mime_type.clone(),
            data: first_image.data.clone(),
        },
    });
    for img in &valid_images[1..] {
        blocks.push(ContentBlock::Image {
            source: Base64ImageSource {
                kind: "base64",
                media_type: img.mime_type.clone(),
                data: img.data.clone(),
            },
        });
    }
    MessageContent::Blocks(blocks)
}

/// assistant 消息 content 出方向形态：纯文本保持字符串，带工具调用时为 block 数组
///
/// 历史消息的 reasoning 字段出方向丢弃（无签名思考块回传会被拒）。
fn assistant_content(msg: &ChatMessage) -> MessageContent {
    let text = msg.content.clone().unwrap_or_default();
    let Some(tool_calls) = &msg.tool_calls else {
        return MessageContent::Text(text);
    };
    let mut blocks = Vec::new();
    if !text.is_empty() {
        blocks.push(ContentBlock::Text {
            text,
            cache_control: None,
        });
    }
    for tc in tool_calls {
        blocks.push(ContentBlock::ToolUse {
            id: tc.id.clone(),
            name: tc.name.clone(),
            input: parse_tool_arguments(&tc.arguments),
        });
    }
    MessageContent::Blocks(blocks)
}

/// 工具参数 JSON 字符串解析为 object，失败（非法 JSON 或非 object）降级空 object
fn parse_tool_arguments(arguments: &str) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .filter(|value| value.is_object())
        .unwrap_or_else(|| serde_json::json!({}))
}

/// 工具结果消息 → tool_result block
fn tool_result_block(msg: &ChatMessage) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: msg.tool_call_id.clone().unwrap_or_default(),
        content: msg.content.clone().unwrap_or_default(),
    }
}

/// 追加消息：与上一条同角色时合并（Anthropic 相邻同角色消息直接 400）
fn push_message(out: &mut Vec<WireMessage>, role: &'static str, content: MessageContent) {
    if let Some(last) = out.last_mut()
        && last.role == role
    {
        merge_content(&mut last.content, content);
        return;
    }
    out.push(WireMessage { role, content });
}

/// 同角色消息内容合并：双字符串拼接保持字符串形态，否则升格为 block 数组拼接
fn merge_content(existing: &mut MessageContent, incoming: MessageContent) {
    match (existing, incoming) {
        (MessageContent::Text(a), MessageContent::Text(b)) => {
            a.push_str("\n\n");
            a.push_str(&b);
        }
        (existing, incoming) => {
            let placeholder = MessageContent::Text(String::new());
            let mut blocks = std::mem::replace(existing, placeholder).into_blocks();
            blocks.extend(incoming.into_blocks());
            *existing = MessageContent::Blocks(blocks);
        }
    }
}

/// 把 block 前插进消息 content（字符串形态先升格为 block 数组）
fn prepend_blocks(content: &mut MessageContent, mut blocks: Vec<ContentBlock>) {
    let placeholder = MessageContent::Text(String::new());
    let mut existing = std::mem::replace(content, placeholder).into_blocks();
    blocks.append(&mut existing);
    *content = MessageContent::Blocks(blocks);
}

/// 提取消息内全部 tool_use 的 id
fn collect_tool_use_ids(msg: &WireMessage) -> Vec<String> {
    let MessageContent::Blocks(blocks) = &msg.content else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

/// 提取消息内全部 tool_result 配对的 tool_use_id 集合
fn collect_tool_result_ids(msg: &WireMessage) -> HashSet<String> {
    let MessageContent::Blocks(blocks) = &msg.content else {
        return HashSet::new();
    };
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect()
}

/// 孤儿 tool_use 兜底：assistant 消息内的 tool_use 若下一条消息未配对 tool_result，
/// 合成中文占位 stub 插进下一条 user 消息最前（无下一条 user 消息则新插一条）
fn backfill_orphaned_tool_uses(messages: &mut Vec<WireMessage>) {
    let mut i = 0;
    while i < messages.len() {
        if messages[i].role != "assistant" {
            i += 1;
            continue;
        }
        let used_ids = collect_tool_use_ids(&messages[i]);
        if used_ids.is_empty() {
            i += 1;
            continue;
        }
        let orphans: Vec<String> = match messages.get(i + 1) {
            Some(next) if next.role == "user" => {
                let answered = collect_tool_result_ids(next);
                used_ids
                    .into_iter()
                    .filter(|id| !answered.contains(id))
                    .collect()
            }
            _ => used_ids,
        };
        if orphans.is_empty() {
            i += 1;
            continue;
        }
        let stubs: Vec<ContentBlock> = orphans
            .into_iter()
            .map(|id| ContentBlock::ToolResult {
                tool_use_id: id,
                content: ORPHAN_TOOL_RESULT_STUB.to_string(),
            })
            .collect();
        let next_is_user = messages.get(i + 1).is_some_and(|next| next.role == "user");
        if next_is_user {
            if let Some(next) = messages.get_mut(i + 1) {
                prepend_blocks(&mut next.content, stubs);
            }
        } else {
            messages.insert(
                i + 1,
                WireMessage {
                    role: "user",
                    content: MessageContent::Blocks(stubs),
                },
            );
        }
        i += 1;
    }
}

/// 领域消息序列 → Anthropic messages 数组（含同角色合并与孤儿兜底）
fn convert_messages(messages: &[ChatMessage]) -> Vec<WireMessage> {
    let mut out = Vec::new();
    for msg in messages {
        if !matches!(msg.role, MessageRole::User) && !msg.images.is_empty() {
            tracing::warn!(role = %msg.role.as_str(), "非 user 消息携带图片，已忽略");
        }
        match msg.role {
            MessageRole::User => push_message(&mut out, "user", user_content(msg)),
            MessageRole::Assistant => {
                push_message(&mut out, "assistant", assistant_content(msg));
            }
            // 工具结果归 user 角色 tool_result block，与相邻 user 内容合并
            MessageRole::Tool => push_message(
                &mut out,
                "user",
                MessageContent::Blocks(vec![tool_result_block(msg)]),
            ),
            // system 角色消息折叠进顶层 system 字段，不进 messages 数组
            MessageRole::System => {}
        }
    }
    backfill_orphaned_tool_uses(&mut out);
    out
}

/// 将内部 ChatRequest 转换为 Anthropic /v1/messages 请求体
///
/// 纯函数：不触碰 HTTP，仅做领域消息 → wire JSON 的翻译。
/// 流式 / 非流式两路径共用，由 `stream` 形参区分是否注入 `stream` 字段。
pub fn build_request_body(
    request: ChatRequest,
    model: &str,
    options: &StreamOptions,
    stream: bool,
) -> serde_json::Value {
    // system 折叠：顶层 system 字段与序列内 system 角色消息合并为一段
    let mut system_parts: Vec<String> = Vec::new();
    if let Some(text) = &request.system {
        system_parts.push(text.clone());
    }
    for msg in &request.messages {
        if matches!(msg.role, MessageRole::System)
            && let Some(text) = &msg.content
            && !text.is_empty()
        {
            system_parts.push(text.clone());
        }
    }
    let system = (!system_parts.is_empty()).then(|| {
        vec![ContentBlock::Text {
            text: system_parts.join("\n\n"),
            cache_control: Some(CacheControl::ephemeral()),
        }]
    });

    let messages = convert_messages(&request.messages);

    // 工具定义转 input_schema 形态，表尾（最后一个工具）恒挂 ephemeral 断点
    let tools = options
        .tools
        .as_ref()
        .filter(|tools| !tools.is_empty())
        .map(|defs| {
            let last = defs.len() - 1;
            defs.iter()
                .enumerate()
                .map(|(i, def)| WireTool {
                    name: def.name.clone(),
                    description: def.description.clone(),
                    input_schema: serde_json::to_value(&def.parameters)
                        .unwrap_or_else(|_| serde_json::json!({"type": "object"})),
                    cache_control: (i == last).then(CacheControl::ephemeral),
                })
                .collect()
        });

    // 该协议暂不支持思考开关：Enabled 打 WARN 后忽略，Disabled 静默忽略
    if matches!(options.thinking_type, Some(ThinkingType::Enabled)) {
        tracing::warn!(model = %model, "Anthropic 协议暂不支持思考开关，已忽略 thinking 配置");
    }

    let body = MessagesRequestBody {
        model: model.to_string(),
        messages,
        max_tokens: MAX_TOKENS,
        system,
        stream,
        tools,
    };
    serde_json::to_value(body).expect("请求体序列化不会失败")
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::{ImageContent, MessageRole, ThinkingType, ToolCallData, ToolDefinition};

    /// 构造纯文本 user 消息
    fn user_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: MessageRole::User,
            content: Some(content.to_string()),
            ..Default::default()
        }
    }

    /// 构造纯文本 assistant 消息
    fn assistant_text_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: MessageRole::Assistant,
            content: Some(content.to_string()),
            ..Default::default()
        }
    }

    /// 构造带单个工具调用的 assistant 消息
    fn assistant_tool_call(id: &str, name: &str, arguments: &str) -> ChatMessage {
        ChatMessage {
            role: MessageRole::Assistant,
            content: None,
            tool_calls: Some(vec![ToolCallData {
                id: id.to_string(),
                name: name.to_string(),
                arguments: arguments.to_string(),
            }]),
            ..Default::default()
        }
    }

    /// 构造工具结果消息
    fn tool_result_msg(id: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: MessageRole::Tool,
            content: Some(content.to_string()),
            tool_call_id: Some(id.to_string()),
            tool_name: Some("任意工具".to_string()),
            ..Default::default()
        }
    }

    /// 不变量检查：产出消息序列永无相邻同角色消息
    fn assert_no_adjacent_same_role(body: &serde_json::Value) {
        let messages = body["messages"].as_array().expect("messages 应为数组");
        for window in messages.windows(2) {
            assert_ne!(
                window[0]["role"], window[1]["role"],
                "出现相邻同角色消息：{window:?}"
            );
        }
    }

    /// 不变量检查：每个 tool_use 必有下一条消息内的配对 tool_result
    fn assert_tool_use_all_paired(body: &serde_json::Value) {
        let messages = body["messages"].as_array().expect("messages 应为数组");
        for (i, msg) in messages.iter().enumerate() {
            let Some(blocks) = msg["content"].as_array() else {
                continue;
            };
            for block in blocks.iter().filter(|b| b["type"] == "tool_use") {
                let id = block["id"].as_str().expect("tool_use 应带 id");
                let paired = messages
                    .get(i + 1)
                    .and_then(|next| next["content"].as_array())
                    .is_some_and(|nb| {
                        nb.iter()
                            .any(|b| b["type"] == "tool_result" && b["tool_use_id"] == id)
                    });
                assert!(paired, "tool_use {id} 缺配对 tool_result");
            }
        }
    }

    #[test]
    fn top_level_fields_and_max_tokens_constant() {
        let request = ChatRequest {
            messages: vec![user_msg("hi")],
            system: None,
        };
        let body = build_request_body(request, "claude-sonnet-4", &StreamOptions::default(), false);

        assert_eq!(body["model"], "claude-sonnet-4");
        // max_tokens 必填且恒发 16384
        assert_eq!(body["max_tokens"], 16384);
        // 非流式不发 stream 字段；tool_choice / reasoning_effort 该协议均不发送
        assert!(body.get("stream").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("reasoning_effort").is_none());

        let body = build_request_body(
            ChatRequest::default(),
            "claude-sonnet-4",
            &StreamOptions::default(),
            true,
        );
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn system_is_top_level_blocks_with_cache_breakpoint() {
        let request = ChatRequest {
            messages: vec![user_msg("hi")],
            system: Some("你是助手".to_string()),
        };
        let body = build_request_body(request, "claude-sonnet-4", &StreamOptions::default(), false);

        // system 是顶层 Blocks 形态且尾挂 ephemeral 断点，不进 messages 数组
        assert_eq!(
            body["system"],
            serde_json::json!([{
                "type": "text",
                "text": "你是助手",
                "cache_control": {"type": "ephemeral"}
            }])
        );
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn pure_text_messages_keep_string_content() {
        let request = ChatRequest {
            messages: vec![user_msg("问"), assistant_text_msg("答")],
            system: None,
        };
        let body = build_request_body(request, "claude-sonnet-4", &StreamOptions::default(), false);

        assert_eq!(body["messages"][0]["content"], "问");
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(body["messages"][1]["content"], "答");
    }

    #[test]
    fn image_block_shape_field_by_field() {
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("看图".to_string()),
                images: vec![ImageContent {
                    mime_type: "image/png".to_string(),
                    data: "aGVsbG8=".to_string(),
                }],
                ..Default::default()
            }],
            system: None,
        };
        let body = build_request_body(request, "claude-sonnet-4", &StreamOptions::default(), false);

        let content = &body["messages"][0]["content"];
        assert!(content.is_array(), "带图消息 content 应为数组");
        assert_eq!(
            content[0],
            serde_json::json!({"type": "text", "text": "看图"})
        );
        assert_eq!(
            content[1],
            serde_json::json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": "aGVsbG8="
                }
            })
        );
    }

    #[test]
    fn image_without_text_gets_placeholder() {
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: None,
                images: vec![ImageContent {
                    mime_type: "image/webp".to_string(),
                    data: "d2VicA==".to_string(),
                }],
                ..Default::default()
            }],
            system: None,
        };
        let body = build_request_body(request, "claude-sonnet-4", &StreamOptions::default(), false);

        let content = &body["messages"][0]["content"];
        let blocks = content.as_array().expect("有图消息 content 应为数组");
        assert_eq!(blocks.len(), 2, "占位文本 + 图片共两个 block");
        assert_eq!(blocks[0]["type"], "text");
        assert!(blocks[0]["text"].as_str().is_some_and(|t| !t.is_empty()));
        assert_eq!(blocks[1]["type"], "image");
    }

    #[test]
    fn unsupported_mime_image_dropped() {
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("文本".to_string()),
                images: vec![
                    ImageContent {
                        mime_type: "application/pdf".to_string(),
                        data: "x".to_string(),
                    },
                    ImageContent {
                        mime_type: "image/png".to_string(),
                        data: "aGVsbG8=".to_string(),
                    },
                ],
                ..Default::default()
            }],
            system: None,
        };
        let body = build_request_body(request, "claude-sonnet-4", &StreamOptions::default(), false);

        let content = &body["messages"][0]["content"];
        let blocks = content.as_array().expect("仍有合法图片，content 应为数组");
        assert_eq!(blocks.len(), 2, "pdf 图应被丢弃");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
    }

    #[test]
    fn oversized_image_dropped_falls_back_to_text() {
        let oversized = "A".repeat(20 * 1024 * 1024 / 3 * 4 + 100);
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: Some("大图".to_string()),
                images: vec![ImageContent {
                    mime_type: "image/png".to_string(),
                    data: oversized,
                }],
                ..Default::default()
            }],
            system: None,
        };
        let body = build_request_body(request, "claude-sonnet-4", &StreamOptions::default(), false);

        assert_eq!(
            body["messages"][0]["content"], "大图",
            "超限图丢弃后回退纯文本"
        );
    }

    #[test]
    fn non_user_images_ignored() {
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::Assistant,
                content: Some("回复".to_string()),
                images: vec![ImageContent {
                    mime_type: "image/png".to_string(),
                    data: "aGVsbG8=".to_string(),
                }],
                ..Default::default()
            }],
            system: None,
        };
        let body = build_request_body(request, "claude-sonnet-4", &StreamOptions::default(), false);

        assert_eq!(body["messages"][0]["content"], "回复");
    }

    #[test]
    fn tool_use_block_shape_and_arguments_fallback() {
        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: MessageRole::Assistant,
                content: Some("调用工具".to_string()),
                reasoning: Some("历史思考".to_string()),
                tool_calls: Some(vec![
                    ToolCallData {
                        id: "tu_1".to_string(),
                        name: "bash".to_string(),
                        arguments: r#"{"cmd":"ls"}"#.to_string(),
                    },
                    ToolCallData {
                        id: "tu_2".to_string(),
                        name: "read".to_string(),
                        arguments: "not-json".to_string(),
                    },
                ]),
                ..Default::default()
            }],
            system: None,
        };
        let body = build_request_body(request, "claude-sonnet-4", &StreamOptions::default(), false);

        let blocks = body["messages"][0]["content"]
            .as_array()
            .expect("带工具调用的 assistant content 应为数组");
        assert_eq!(
            blocks[0],
            serde_json::json!({"type": "text", "text": "调用工具"})
        );
        assert_eq!(
            blocks[1],
            serde_json::json!({
                "type": "tool_use",
                "id": "tu_1",
                "name": "bash",
                "input": {"cmd": "ls"}
            })
        );
        // 非法 JSON 参数降级空 object
        assert_eq!(
            blocks[2],
            serde_json::json!({
                "type": "tool_use",
                "id": "tu_2",
                "name": "read",
                "input": {}
            })
        );
        // 历史消息的 reasoning 字段出方向丢弃（精确等值已锁定无多余字段）
        assert!(body["messages"][0].get("reasoning").is_none());
    }

    #[test]
    fn tool_result_block_shape() {
        let request = ChatRequest {
            messages: vec![
                assistant_tool_call("tu_1", "bash", "{}"),
                tool_result_msg("tu_1", "命令输出"),
            ],
            system: None,
        };
        let body = build_request_body(request, "claude-sonnet-4", &StreamOptions::default(), false);

        // 工具结果转 user 角色 tool_result block
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(
            body["messages"][1]["content"],
            serde_json::json!([{
                "type": "tool_result",
                "tool_use_id": "tu_1",
                "content": "命令输出"
            }])
        );
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_message() {
        let request = ChatRequest {
            messages: vec![
                ChatMessage {
                    role: MessageRole::Assistant,
                    content: None,
                    tool_calls: Some(vec![
                        ToolCallData {
                            id: "tu_1".to_string(),
                            name: "bash".to_string(),
                            arguments: "{}".to_string(),
                        },
                        ToolCallData {
                            id: "tu_2".to_string(),
                            name: "read".to_string(),
                            arguments: "{}".to_string(),
                        },
                    ]),
                    ..Default::default()
                },
                tool_result_msg("tu_1", "r1"),
                tool_result_msg("tu_2", "r2"),
            ],
            system: None,
        };
        let body = build_request_body(request, "claude-sonnet-4", &StreamOptions::default(), false);

        assert_eq!(
            body["messages"].as_array().map(Vec::len),
            Some(2),
            "连续工具结果应合并进同一条 user 消息"
        );
        let blocks = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["tool_use_id"], "tu_1");
        assert_eq!(blocks[1]["tool_use_id"], "tu_2");
        assert_no_adjacent_same_role(&body);
    }

    #[test]
    fn user_message_after_tool_results_merges_into_same_user_message() {
        let request = ChatRequest {
            messages: vec![
                assistant_tool_call("tu_1", "bash", "{}"),
                tool_result_msg("tu_1", "r1"),
                user_msg("下一步"),
            ],
            system: None,
        };
        let body = build_request_body(request, "claude-sonnet-4", &StreamOptions::default(), false);

        assert_eq!(
            body["messages"].as_array().map(Vec::len),
            Some(2),
            "工具结果后紧跟的 user 消息应合并进同一条"
        );
        let blocks = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(
            blocks[1],
            serde_json::json!({"type": "text", "text": "下一步"})
        );
    }

    #[test]
    fn no_adjacent_same_role_for_arbitrary_inputs() {
        // 覆盖连续 user、连续 assistant、连续 tool、tool 后紧跟 user 的任意序列
        let request = ChatRequest {
            messages: vec![
                user_msg("第一问"),
                user_msg("第二问"),
                assistant_text_msg("文本回答"),
                assistant_tool_call("tu_1", "bash", "{}"),
                tool_result_msg("tu_1", "r1"),
                tool_result_msg("tu_2", "r2"),
                user_msg("收尾"),
            ],
            system: None,
        };
        let body = build_request_body(request, "claude-sonnet-4", &StreamOptions::default(), false);

        assert_no_adjacent_same_role(&body);
        // 结构抽查：连续 user 合并、连续 assistant 合并、tool + user 全并入一条 user
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(3));
        assert_eq!(body["messages"][0]["content"], "第一问\n\n第二问");
        let assistant_blocks = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(assistant_blocks.len(), 2, "文本 + 工具调用合并");
        let user_blocks = body["messages"][2]["content"].as_array().unwrap();
        assert_eq!(user_blocks.len(), 3, "两个工具结果 + 文本合并");
    }

    #[test]
    fn every_tool_use_paired_orphan_gets_stub() {
        // 两种孤儿场景：下一条 user 消息无配对、历史在工具完成前截断
        let request = ChatRequest {
            messages: vec![
                assistant_tool_call("tu_a", "bash", "{}"),
                tool_result_msg("tu_a", "r"),
                assistant_tool_call("tu_b", "read", "{}"),
                user_msg("继续"),
                assistant_tool_call("tu_c", "web", "{}"),
            ],
            system: None,
        };
        let body = build_request_body(request, "claude-sonnet-4", &StreamOptions::default(), false);

        assert_tool_use_all_paired(&body);
        assert_no_adjacent_same_role(&body);
        // stub 内容锁定：插在下一条 user 消息最前
        assert_eq!(
            body["messages"][3]["content"][0],
            serde_json::json!({
                "type": "tool_result",
                "tool_use_id": "tu_b",
                "content": "工具结果缺失：该轮次在工具完成前被中断"
            })
        );
        // 末尾孤儿：新插一条 user 消息承接 stub
        assert_eq!(body["messages"][5]["role"], "user");
        assert_eq!(body["messages"][5]["content"][0]["tool_use_id"], "tu_c");
        assert_eq!(
            body["messages"][5]["content"][0]["content"],
            "工具结果缺失：该轮次在工具完成前被中断"
        );
    }

    #[test]
    fn thinking_enabled_warns_and_omits_field() {
        let options = StreamOptions {
            thinking_type: Some(ThinkingType::Enabled),
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        let body = build_request_body(ChatRequest::default(), "claude-sonnet-4", &options, false);

        assert!(
            body.get("thinking").is_none(),
            "Enabled 忽略，不发 thinking 字段"
        );
        assert!(
            body.get("reasoning_effort").is_none(),
            "该协议无思考强度概念"
        );

        let options = StreamOptions {
            thinking_type: Some(ThinkingType::Disabled),
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        let body = build_request_body(ChatRequest::default(), "claude-sonnet-4", &options, false);

        assert!(body.get("thinking").is_none(), "Disabled 同样不发字段");
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn empty_tools_omits_field_and_last_tool_gets_cache_breakpoint() {
        let options = StreamOptions {
            tools: Some(vec![]),
            ..Default::default()
        };
        let body = build_request_body(ChatRequest::default(), "claude-sonnet-4", &options, false);
        assert!(body.get("tools").is_none(), "空工具表不发 tools 字段");

        let options = StreamOptions {
            tools: Some(vec![
                ToolDefinition::new("bash", "执行命令"),
                ToolDefinition::new("read", "读取文件"),
            ]),
            ..Default::default()
        };
        let body = build_request_body(ChatRequest::default(), "claude-sonnet-4", &options, false);

        let tools = body["tools"].as_array().expect("有工具时应发 tools 字段");
        assert_eq!(
            tools[0],
            serde_json::json!({
                "name": "bash",
                "description": "执行命令",
                "input_schema": {"type": "object", "properties": {}, "required": []}
            }),
            "非表尾工具不挂缓存断点"
        );
        assert_eq!(
            tools[1]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
    }
}
