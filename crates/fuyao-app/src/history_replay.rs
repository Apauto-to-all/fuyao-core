//! 历史回放：存储 Message → OutputEvent 投影
//!
//! 把会话历史消息（持久化的 [`Message`]）投影成与实时流同构的 [`OutputEvent`]，
//! 供前端历史回放——前端只认一种渲染模型（OutputEvent），无需区分「实时流来的」
//! 还是「从存储读出的」，UI 渲染层零特殊处理。
//!
//! # 为何放在 core
//!
//! `Message.tool_calls` 落库时被序列化成 OpenAI 嵌套形态
//!（`{id, type:"function", function:{name, arguments}}`，见 core 写库处的构造），
//! 而 [`OutputEvent::Assistant`] 的 `tool_calls` 是扁平 [`ToolCallPayload`]。
//! 「嵌套 → 扁平」的逆向是 core 自身存储实现细节的内部知识，由 core 持有：
//! 放前端会泄漏 core 存储格式；放适配层违反「业务逻辑零渗入」。core 是该转换的唯一归属。
//!
//! # 存储模型固有限制
//!
//! `Message` 未持久化用户消息的 `mode` / `source`（DB schema 无此列），回放时统一按
//! 普通用户消息兜底（`mode = Guide`、`source = User`）。这是「给人看的历史浏览」的
//! 合理近似——插件 / 系统注入的 user 消息回放成普通 user，不影响可读性。

use fuyao_api::message::EventBase;
use fuyao_api::message::input::{UserMessageMode, UserMessageSource};
use fuyao_api::message::output::{
    AssistantMessage, AssistantPayload, OutputEvent, ToolResultMessage, ToolResultPayload,
    UserMessage, UserPayload,
};
use fuyao_api::{Message, MessageRole};

// ── 单条投影 ─────────────────────────────────────────────────

/// 单条历史 Message → OutputEvent
///
/// 按 [`MessageRole`] 分流到 User / Assistant / ToolResult；`system` 无对应事件变体，
/// 返回 `None`（系统提示是构造而非对话内容，不进历史回放流）。
///
/// `base` 复刻原消息的 timestamp / session_id，仅 id 合成稳定串 `hist-{seq}`——
/// 历史消息无原始 OutputEvent 的 UUID，但 seq 在会话内唯一，足以作渲染 key。
fn message_to_event(msg: &Message) -> Option<OutputEvent> {
    // base 复刻原消息时间戳与会话标识；id 用 seq 合成稳定串供前端作渲染 key
    let base = EventBase {
        id: format!("hist-{}", msg.seq),
        timestamp: msg.timestamp,
        session_id: Some(msg.session_id.clone()),
    };
    match msg.role {
        MessageRole::User => Some(OutputEvent::User(UserMessage {
            base,
            payload: UserPayload {
                content: msg.content.clone().unwrap_or_default(),
                images: msg.images.clone(),
                // mode / source 未持久化，按普通用户消息兜底
                mode: UserMessageMode::Guide,
                source: UserMessageSource::User,
            },
        })),
        MessageRole::Assistant => Some(OutputEvent::Assistant(AssistantMessage {
            base,
            payload: AssistantPayload {
                content: msg.content.clone(),
                reasoning: msg.reasoning.clone(),
                tool_calls: parse_tool_calls(msg.tool_calls.as_ref()),
                finish_reason: msg.finish_reason.clone(),
                completion_tokens: msg.completion_tokens,
                prompt_tokens: msg.prompt_tokens,
                // Message 未单独存 total_tokens，按 prompt + completion 求和近似
                total_tokens: msg.prompt_tokens + msg.completion_tokens,
                reasoning_tokens: msg.reasoning_tokens,
                cached_tokens: msg.cached_tokens,
            },
        })),
        MessageRole::Tool => Some(OutputEvent::ToolResult(ToolResultMessage {
            base,
            payload: ToolResultPayload {
                tool_call_id: msg.tool_call_id.clone().unwrap_or_default(),
                tool_name: msg.tool_name.clone().unwrap_or_default(),
                content: msg.content.clone().unwrap_or_default(),
            },
        })),
        // 系统消息是 prompt 构造，非对话内容，不进历史回放流
        MessageRole::System => None,
    }
}

/// OpenAI 嵌套 tool_calls → 扁平 ToolCallPayload 列表
///
/// 落库形态（与 core 写库处一致）：
/// ```json
/// [{ "id": "call_1", "type": "function",
///    "function": { "name": "search", "arguments": "{\"q\":\"rust\"}" } }]
/// ```
/// - `arguments` 是 **JSON 字符串**（OpenAI 协议原样），需 `from_str` 解析成对象赋给
///   `tool_args`（[`ToolCallPayload::tool_args`] 是 `serde_json::Value`）；解析失败兜底成
///   原字符串 Value，保证信息不丢。
/// - `type: "function"` 是 OpenAI 协议标记，扁平形态不需要，丢弃。
///
/// 非数组 / 元素缺字段时跳过该元素（容错），不整体失败——单条工具调用损坏不应阻断整段历史。
fn parse_tool_calls(
    tool_calls: Option<&serde_json::Value>,
) -> Option<Vec<fuyao_api::message::output::ToolCallPayload>> {
    let arr = tool_calls?.as_array()?;
    let parsed: Vec<_> = arr.iter().filter_map(parse_tool_call).collect();
    if parsed.is_empty() {
        None
    } else {
        Some(parsed)
    }
}

/// 单条 OpenAI 嵌套 tool_call → 扁平 ToolCallPayload
///
/// 字段映射：`id → tool_call_id`、`function.name → tool_name`、
/// `function.arguments(字符串) → tool_args(JSON Value)`。
fn parse_tool_call(v: &serde_json::Value) -> Option<fuyao_api::message::output::ToolCallPayload> {
    use fuyao_api::message::output::ToolCallPayload;

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

// ── 批量投影 ─────────────────────────────────────────────────

/// 历史消息列表 → 事件流（seq 正序，旧 → 新）
///
/// 接收「seq 倒序」（存储默认查询顺序，最新在前）的 [`Message`] 列表，反向遍历投影
/// 得正序（旧在前、新在后），使历史回放流与实时流时序一致。
///
/// 游标分页标准模式：存储层 `ORDER BY seq DESC`（倒序取数利于游标定位边界），业务层
/// 翻成正序返回——用户看对话是旧→新。`.iter().rev()` 反向遍历 DESC 输入即得 ASC，
/// 一次到位，不再额外翻转。
///
/// `system` 消息投影为 `None` 会被跳过，故返回长度可能小于输入。
pub(crate) fn messages_to_events(messages: Vec<Message>) -> Vec<OutputEvent> {
    messages.iter().rev().filter_map(message_to_event).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::ImageContent;
    use fuyao_api::message::output::ToolCallPayload;

    /// 构造一条 Message（零值兜底，按 role / 覆盖字段微调）
    fn make_msg(role: MessageRole, seq: i64, overrides: &dyn Fn(&mut Message)) -> Message {
        let mut msg = Message {
            id: Some(seq),
            session_id: "sess-1".into(),
            model_id: None,
            role,
            content: None,
            images: vec![],
            reasoning: None,
            tool_call_id: None,
            tool_calls: None,
            tool_name: None,
            finish_reason: None,
            timestamp: seq as f64,
            prompt_tokens: 0,
            completion_tokens: 0,
            reasoning_tokens: 0,
            cached_tokens: 0,
            cost: 0.0,
            seq,
            kind: fuyao_api::MessageKind::Message,
        };
        overrides(&mut msg);
        msg
    }

    #[test]
    fn user_message_projects_with_default_mode_source() {
        let msg = make_msg(MessageRole::User, 1, &|m| {
            m.content = Some("你好".into());
        });

        let event = message_to_event(&msg).expect("user 应投影成事件");
        let OutputEvent::User(UserMessage { base, payload }) = event else {
            panic!("应为 User 变体，实际：{event:?}");
        };
        assert_eq!(payload.content, "你好");
        // mode / source 未持久化，兜底成 Guide / User
        assert_eq!(payload.mode, UserMessageMode::Guide);
        assert_eq!(payload.source, UserMessageSource::User);
        // base 复刻原消息时间戳与会话标识
        assert!((base.timestamp - 1.0).abs() < f64::EPSILON);
        assert_eq!(base.session_id.as_deref(), Some("sess-1"));
        // id 用 seq 合成稳定串
        assert_eq!(base.id, "hist-1");
    }

    #[test]
    fn user_message_carries_images() {
        let img = ImageContent {
            mime_type: "image/png".into(),
            data: "iVBORw0".into(),
        };
        let msg = make_msg(MessageRole::User, 2, &|m| {
            m.images = vec![img.clone()];
        });

        let OutputEvent::User(UserMessage { payload, .. }) =
            message_to_event(&msg).expect("user 应投影")
        else {
            panic!("变体类型不符");
        };
        assert_eq!(payload.images.len(), 1);
        assert_eq!(payload.images[0].mime_type, "image/png");
    }

    #[test]
    fn user_empty_content_defaults_to_empty_string() {
        // content = None 时 UserPayload.content 兜底空串（而非 panic）
        let msg = make_msg(MessageRole::User, 3, &|_| {});
        let OutputEvent::User(UserMessage { payload, .. }) =
            message_to_event(&msg).expect("user 应投影")
        else {
            panic!("变体类型不符");
        };
        assert_eq!(payload.content, "");
    }

    #[test]
    fn assistant_message_projects_tokens_and_content() {
        let msg = make_msg(MessageRole::Assistant, 4, &|m| {
            m.content = Some("回复".into());
            m.reasoning = Some("思考".into());
            m.finish_reason = Some("stop".into());
            m.prompt_tokens = 20;
            m.completion_tokens = 10;
            m.reasoning_tokens = 5;
            m.cached_tokens = 2;
        });

        let OutputEvent::Assistant(AssistantMessage { payload, .. }) =
            message_to_event(&msg).expect("assistant 应投影")
        else {
            panic!("变体类型不符");
        };
        assert_eq!(payload.content.as_deref(), Some("回复"));
        assert_eq!(payload.reasoning.as_deref(), Some("思考"));
        assert_eq!(payload.finish_reason.as_deref(), Some("stop"));
        assert_eq!(payload.prompt_tokens, 20);
        assert_eq!(payload.completion_tokens, 10);
        // total_tokens = prompt + completion 近似
        assert_eq!(payload.total_tokens, 30);
        assert_eq!(payload.reasoning_tokens, 5);
        assert_eq!(payload.cached_tokens, 2);
        // 无 tool_calls
        assert!(payload.tool_calls.is_none());
    }

    #[test]
    fn assistant_tool_calls_openai_nested_to_flat() {
        // 落库形态：OpenAI 嵌套，arguments 是 JSON 字符串
        let tool_calls = serde_json::json!([
            {
                "id": "call_1",
                "type": "function",
                "function": { "name": "search", "arguments": "{\"q\":\"rust\"}" }
            },
            {
                "id": "call_2",
                "type": "function",
                "function": { "name": "read", "arguments": "{\"path\":\"/a.rs\"}" }
            }
        ]);
        let msg = make_msg(MessageRole::Assistant, 5, &|m| {
            m.tool_calls = Some(tool_calls.clone());
        });

        let OutputEvent::Assistant(AssistantMessage { payload, .. }) =
            message_to_event(&msg).expect("assistant 应投影")
        else {
            panic!("变体类型不符");
        };
        let calls = payload.tool_calls.expect("应有 tool_calls");
        assert_eq!(calls.len(), 2);

        assert_eq!(calls[0].tool_call_id, "call_1");
        assert_eq!(calls[0].tool_name, "search");
        // arguments 字符串应解析成 JSON 对象
        assert_eq!(calls[0].tool_args, serde_json::json!({"q": "rust"}));

        assert_eq!(calls[1].tool_call_id, "call_2");
        assert_eq!(calls[1].tool_name, "read");
        assert_eq!(calls[1].tool_args, serde_json::json!({"path": "/a.rs"}));
    }

    #[test]
    fn assistant_invalid_arguments_string_falls_back_to_raw() {
        // arguments 非 JSON（解析失败）→ 兜底成原字符串 Value，信息不丢
        let tool_calls = serde_json::json!([{
            "id": "call_x",
            "type": "function",
            "function": { "name": "bad", "arguments": "not-json" }
        }]);
        let msg = make_msg(MessageRole::Assistant, 6, &|m| {
            m.tool_calls = Some(tool_calls.clone());
        });

        let OutputEvent::Assistant(AssistantMessage { payload, .. }) =
            message_to_event(&msg).expect("assistant 应投影")
        else {
            panic!("变体类型不符");
        };
        let call = &payload.tool_calls.expect("应有 tool_calls")[0];
        assert_eq!(call.tool_name, "bad");
        // 解析失败兜底：原字符串作为 Value::String
        assert_eq!(call.tool_args, serde_json::Value::String("not-json".into()));
    }

    #[test]
    fn assistant_malformed_tool_call_element_skipped() {
        // 缺 id 的元素应被跳过，不阻断其余合法元素
        let tool_calls = serde_json::json!([
            { "type": "function", "function": { "name": "no_id", "arguments": "{}" } },
            { "id": "call_ok", "type": "function",
              "function": { "name": "ok", "arguments": "{}" } }
        ]);
        let msg = make_msg(MessageRole::Assistant, 7, &|m| {
            m.tool_calls = Some(tool_calls.clone());
        });

        let OutputEvent::Assistant(AssistantMessage { payload, .. }) =
            message_to_event(&msg).expect("assistant 应投影")
        else {
            panic!("变体类型不符");
        };
        let calls = payload.tool_calls.expect("应有 tool_calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool_call_id, "call_ok");
    }

    #[test]
    fn assistant_empty_tool_calls_array_becomes_none() {
        // 空数组投影成 None（而非空 Vec），与流式事件「无工具调用」语义一致
        let msg = make_msg(MessageRole::Assistant, 8, &|m| {
            m.tool_calls = Some(serde_json::json!([]));
        });
        let OutputEvent::Assistant(AssistantMessage { payload, .. }) =
            message_to_event(&msg).expect("assistant 应投影")
        else {
            panic!("变体类型不符");
        };
        assert!(payload.tool_calls.is_none());
    }

    #[test]
    fn tool_message_projects_to_tool_result() {
        let msg = make_msg(MessageRole::Tool, 9, &|m| {
            m.tool_call_id = Some("call_1".into());
            m.tool_name = Some("get_weather".into());
            m.content = Some("sunny".into());
        });

        let OutputEvent::ToolResult(ToolResultMessage { base, payload }) =
            message_to_event(&msg).expect("tool 应投影")
        else {
            panic!("变体类型不符");
        };
        assert_eq!(payload.tool_call_id, "call_1");
        assert_eq!(payload.tool_name, "get_weather");
        assert_eq!(payload.content, "sunny");
        assert_eq!(base.id, "hist-9");
    }

    #[test]
    fn tool_message_missing_fields_default_empty() {
        // tool_call_id / tool_name / content 缺失时兜底空串，不 panic
        let msg = make_msg(MessageRole::Tool, 10, &|_| {});
        let OutputEvent::ToolResult(ToolResultMessage { payload, .. }) =
            message_to_event(&msg).expect("tool 应投影")
        else {
            panic!("变体类型不符");
        };
        assert_eq!(payload.tool_call_id, "");
        assert_eq!(payload.tool_name, "");
        assert_eq!(payload.content, "");
    }

    #[test]
    fn system_message_is_skipped() {
        // system 无对应事件变体，投影成 None
        let msg = make_msg(MessageRole::System, 11, &|m| {
            m.content = Some("你是助手".into());
        });
        assert!(message_to_event(&msg).is_none());
    }

    #[test]
    fn messages_to_events_reverses_desc_to_asc() {
        // 存储默认 seq 倒序（最新在前）；批量投影应翻成正序（旧在前）。
        // 用 seq 合成的 base.id（hist-{seq}）断言真实顺序——两端类型相同也能区分，
        // 避免此前「User→Assistant→User 类型序列翻不翻转都成立」的无效断言。
        let messages = vec![
            make_msg(MessageRole::User, 3, &|m| {
                m.content = Some("三".into());
            }),
            make_msg(MessageRole::Assistant, 2, &|m| {
                m.content = Some("二".into());
            }),
            make_msg(MessageRole::User, 1, &|m| {
                m.content = Some("一".into());
            }),
        ];

        let events = messages_to_events(messages);
        assert_eq!(events.len(), 3);
        // 正序：seq 1 → 2 → 3，用 base.id 锁死顺序
        let seqs: Vec<&str> = events
            .iter()
            .map(|e| match e {
                OutputEvent::User(m) => m.base.id.as_str(),
                OutputEvent::Assistant(m) => m.base.id.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(seqs, vec!["hist-1", "hist-2", "hist-3"]);
    }

    #[test]
    fn messages_to_events_skips_system() {
        // system 在批量投影中被过滤
        let messages = vec![
            make_msg(MessageRole::System, 2, &|_| {}),
            make_msg(MessageRole::User, 1, &|m| {
                m.content = Some("用户".into());
            }),
        ];

        let events = messages_to_events(messages);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], OutputEvent::User(_)));
    }

    #[test]
    fn messages_to_events_empty_input() {
        assert!(messages_to_events(vec![]).is_empty());
    }

    /// 静态断言：ToolCallPayload 来自 fuyao_api（避免误以为本地类型，锁死导入路径）
    #[test]
    fn tool_call_payload_imported_from_fuyao_api() {
        let _ = ToolCallPayload {
            tool_call_id: String::new(),
            tool_name: String::new(),
            tool_args: serde_json::Value::Null,
        };
    }
}
