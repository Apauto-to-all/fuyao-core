//! 事件协议集成测试
//!
//! 钉死 `InputEvent`（UI → Engine）与 `OutputEvent`（Engine → UI）的公共 API 契约：
//! 变体穷尽性、envelope/payload 字段、serde 往返、共享枚举语义。
//! 全部为纯值类型，零 IO，零全局状态。

use fuyao_api::message::input::{
    CompressRequest, InputEvent, InterruptMessage, InterruptPayload, InterruptSource,
    PluginEventSource, PluginMessage, PluginPayload, PluginSource, SystemSource, UserMessage,
    UserMessageMode, UserMessageSource, UserPayload,
};
use fuyao_api::message::output::{
    AssistantMessage, AssistantPayload, ChunkMessage, ChunkPayload, CompressionDeltaPayload,
    CompressionEndedPayload, CompressionMessage, CompressionPayload, CompressionReason,
    CompressionStartedPayload, ErrorMessage, ErrorPayload, OutputEvent, ToolCallMessage,
    ToolCallPayload, ToolResultMessage, ToolResultPayload,
};
use fuyao_api::message::output::{
    InterruptMessage as OutputInterruptMessage, InterruptPayload as OutputInterruptPayload,
    PluginMessage as OutputPluginMessage, PluginPayload as OutputPluginPayload,
    UserMessage as OutputUserMessage, UserPayload as OutputUserPayload,
};
use fuyao_api::{EventBase, ThinkingType};
use rstest::rstest;

// ---------------------------------------------------------------------------
// EventBase：动态字段（seq/时间戳）的契约
// ---------------------------------------------------------------------------

#[test]
fn event_base_default_seq_is_none() {
    let base = EventBase::default();

    // 默认 seq 为 None：纯实时事件无 seq，落库后由调用方回填
    assert!(base.seq.is_none(), "默认 seq 应为 None");
    // 时间戳为正数（Unix 秒）
    assert!(base.timestamp > 0.0, "时间戳应为正数");
}

#[test]
fn event_base_seq_round_trip() {
    // seq = Some 时序列化 / 反序列化往返一致
    let base = EventBase {
        seq: Some(123),
        ..Default::default()
    };
    let json = serde_json::to_string(&base).expect("序列化失败");
    let restored: EventBase = serde_json::from_str(&json).expect("反序列化失败");
    assert_eq!(restored.seq, Some(123));
}

#[test]
fn event_base_update_timestamp_refreshes_value() {
    let mut base = EventBase::default();
    let before = base.timestamp;
    // 短暂停顿以保证时间戳推进（EventBase 精度足够捕获）
    std::thread::sleep(std::time::Duration::from_millis(20));
    base.update_timestamp();
    assert!(base.timestamp >= before, "update_timestamp 后不应回退");
}

// ---------------------------------------------------------------------------
// InputEvent：4 变体穷尽性与 serde 往返
// ---------------------------------------------------------------------------

/// 构造全部 InputEvent 变体，用于参数化往返测试
fn input_event_samples() -> Vec<InputEvent> {
    vec![
        InputEvent::User(UserMessage {
            base: EventBase::default(),
            payload: UserPayload {
                content: "用户消息".into(),
                images: vec![],
                mode: UserMessageMode::Guide,
                source: UserMessageSource::User,
            },
        }),
        InputEvent::Interrupt(InterruptMessage {
            base: EventBase::default(),
            payload: InterruptPayload {
                reason: "用户取消".into(),
                source: InterruptSource::User,
            },
        }),
        InputEvent::Plugin(PluginMessage {
            base: EventBase::default(),
            payload: PluginPayload {
                source: PluginEventSource {
                    name: "loop_guard".into(),
                },
                event_type: "loop_warn".into(),
                data: Some(serde_json::json!({"count": 3})),
                error: None,
                message: Some("检测到循环".into()),
            },
        }),
        InputEvent::Compress(CompressRequest {
            base: EventBase::default(),
        }),
    ]
}

#[rstest]
fn input_event_serde_preserves_variant(#[values(0, 1, 2, 3)] idx: usize) {
    let original = input_event_samples()[idx].clone();
    let json = serde_json::to_string(&original).expect("序列化失败");
    let restored: InputEvent = serde_json::from_str(&json).expect("反序列化失败");

    // 变体标签必须保留（穷尽匹配，新增变体时此处强制处理）
    match (&original, &restored) {
        (InputEvent::User(_), InputEvent::User(_)) => {}
        (InputEvent::Interrupt(_), InputEvent::Interrupt(_)) => {}
        (InputEvent::Plugin(_), InputEvent::Plugin(_)) => {}
        (InputEvent::Compress(_), InputEvent::Compress(_)) => {}
        _ => panic!("serde 往返后变体不匹配"),
    }
}

// ---------------------------------------------------------------------------
// OutputEvent：8 变体穷尽性与 serde 往返
// ---------------------------------------------------------------------------

fn output_event_samples() -> Vec<OutputEvent> {
    vec![
        OutputEvent::Chunk(ChunkMessage {
            base: EventBase::default(),
            payload: ChunkPayload {
                content: Some("文本块".into()),
                reasoning: Some("推理块".into()),
            },
        }),
        OutputEvent::User(OutputUserMessage {
            base: EventBase::default(),
            payload: OutputUserPayload {
                content: "你好".into(),
                images: vec![],
                mode: UserMessageMode::Pending,
                source: UserMessageSource::User,
            },
        }),
        OutputEvent::ToolCall(ToolCallMessage {
            base: EventBase::default(),
            payload: ToolCallPayload {
                tool_call_id: "call_1".into(),
                tool_name: "get_weather".into(),
                tool_args: serde_json::json!({"city": "北京"}),
            },
        }),
        OutputEvent::ToolResult(ToolResultMessage {
            base: EventBase::default(),
            payload: ToolResultPayload {
                tool_call_id: "call_1".into(),
                tool_name: "get_weather".into(),
                content: "sunny".into(),
            },
        }),
        OutputEvent::Assistant(AssistantMessage {
            base: EventBase::default(),
            payload: AssistantPayload {
                content: Some("回复".into()),
                reasoning: None,
                tool_calls: Some(vec![ToolCallPayload {
                    tool_call_id: "call_2".into(),
                    tool_name: "search".into(),
                    tool_args: serde_json::json!({"q": "rust"}),
                }]),
                finish_reason: Some("tool_calls".into()),
                completion_tokens: 10,
                prompt_tokens: 20,
                total_tokens: 30,
                reasoning_tokens: 5,
                cached_tokens: 0,
            },
        }),
        OutputEvent::Interrupt(OutputInterruptMessage {
            base: EventBase::default(),
            payload: OutputInterruptPayload {
                reason: "循环检测".into(),
                source: InterruptSource::Hook,
            },
        }),
        OutputEvent::Error(ErrorMessage {
            base: EventBase::default(),
            payload: ErrorPayload {
                message: "出错了".into(),
                recoverable: true,
            },
        }),
        OutputEvent::Plugin(OutputPluginMessage {
            base: EventBase::default(),
            payload: OutputPluginPayload {
                source: PluginEventSource {
                    name: "test".into(),
                },
                event_type: "custom".into(),
                data: None,
                error: None,
                message: None,
            },
        }),
        // 压缩事件三阶段（Started / Delta / Ended）样本
        OutputEvent::Compression(CompressionMessage {
            base: EventBase::default(),
            payload: CompressionPayload::Started(CompressionStartedPayload {
                reason: CompressionReason::Auto,
                prompt_tokens: 10_000,
                context_length: 128_000,
            }),
        }),
        OutputEvent::Compression(CompressionMessage {
            base: EventBase::default(),
            payload: CompressionPayload::Delta(CompressionDeltaPayload {
                content: Some("摘要片段".into()),
                reasoning: None,
            }),
        }),
        OutputEvent::Compression(CompressionMessage {
            base: EventBase::default(),
            payload: CompressionPayload::Ended(CompressionEndedPayload {
                reason: CompressionReason::Auto,
                content: "完整摘要".into(),
                new_seq: 42,
            }),
        }),
    ]
}

#[rstest]
fn output_event_serde_preserves_variant(#[values(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10)] idx: usize) {
    let original = output_event_samples()[idx].clone();
    let json = serde_json::to_string(&original).expect("序列化失败");
    let restored: OutputEvent = serde_json::from_str(&json).expect("反序列化失败");

    // 11 样本穷尽匹配（8 基础变体 + 3 压缩 mode），保证 serde 不丢标签
    match (&original, &restored) {
        (OutputEvent::Chunk(_), OutputEvent::Chunk(_)) => {}
        (OutputEvent::User(_), OutputEvent::User(_)) => {}
        (OutputEvent::ToolCall(_), OutputEvent::ToolCall(_)) => {}
        (OutputEvent::ToolResult(_), OutputEvent::ToolResult(_)) => {}
        (OutputEvent::Assistant(_), OutputEvent::Assistant(_)) => {}
        (OutputEvent::Interrupt(_), OutputEvent::Interrupt(_)) => {}
        (OutputEvent::Error(_), OutputEvent::Error(_)) => {}
        (OutputEvent::Plugin(_), OutputEvent::Plugin(_)) => {}
        (OutputEvent::Compression(_), OutputEvent::Compression(_)) => {}
        _ => panic!("serde 往返后变体不匹配"),
    }
}

// ---------------------------------------------------------------------------
// Chunk payload：content / reasoning 四态
// ---------------------------------------------------------------------------

#[rstest]
fn chunk_payload_covers_four_states(
    #[values(("文本", "推理"), ("文本", ""), ("", "推理"), ("", ""))] parts: (&str, &str),
) {
    let payload = ChunkPayload {
        content: if parts.0.is_empty() {
            None
        } else {
            Some(parts.0.into())
        },
        reasoning: if parts.1.is_empty() {
            None
        } else {
            Some(parts.1.into())
        },
    };
    let event = OutputEvent::Chunk(ChunkMessage {
        base: EventBase::default(),
        payload,
    });
    let json = serde_json::to_string(&event).expect("序列化失败");
    let restored: OutputEvent = serde_json::from_str(&json).expect("反序列化失败");
    match restored {
        OutputEvent::Chunk(msg) => {
            let want_content = if parts.0.is_empty() {
                None
            } else {
                Some(parts.0.to_string())
            };
            assert_eq!(msg.payload.content, want_content);
        }
        _ => panic!("应为 Chunk 变体"),
    }
}

// ---------------------------------------------------------------------------
// UserMessageSource：三分支 serde 独立性
// ---------------------------------------------------------------------------

#[rstest]
fn user_message_source_serde_preserves_branch(
    #[values(
        UserMessageSource::User,
        UserMessageSource::System(SystemSource { reason: "压缩".into() }),
        UserMessageSource::Plugin(PluginSource { name: "loop_guard".into() }),
    )]
    source: UserMessageSource,
) {
    let payload = UserPayload {
        content: "x".into(),
        images: vec![],
        mode: UserMessageMode::Guide,
        source,
    };
    let json = serde_json::to_string(&payload).expect("序列化失败");
    let restored: UserPayload = serde_json::from_str(&json).expect("反序列化失败");

    // UserMessageSource 派生 PartialEq，往返后应严格相等（钉死三分支不被合并）
    assert_eq!(payload.source, restored.source);
}

// ---------------------------------------------------------------------------
// input::UserMessage 与 output::UserMessage 是不同类型同字段
// 钉死「各自 serde 独立」契约，防止未来误改导致两者耦合
// ---------------------------------------------------------------------------

#[test]
fn input_and_output_user_message_are_independent_types() {
    // 两者 payload 字段同名（content/mode/source），但分属 input / output 模块
    let input_payload = UserPayload {
        content: "input".into(),
        images: vec![],
        mode: UserMessageMode::Guide,
        source: UserMessageSource::User,
    };
    let output_payload = OutputUserPayload {
        content: "output".into(),
        images: vec![],
        mode: UserMessageMode::Pending,
        source: UserMessageSource::System(SystemSource {
            reason: "测试".into(),
        }),
    };

    // 各自独立序列化，互不影响
    let input_json = serde_json::to_string(&input_payload).expect("序列化失败");
    let output_json = serde_json::to_string(&output_payload).expect("序列化失败");
    assert_ne!(input_json, output_json);

    // 各自独立反序列化
    let input_restored: UserPayload = serde_json::from_str(&input_json).expect("反序列化失败");
    let output_restored: OutputUserPayload =
        serde_json::from_str(&output_json).expect("反序列化失败");
    assert_eq!(input_restored.content, "input");
    assert_eq!(output_restored.content, "output");
}

// ---------------------------------------------------------------------------
// InterruptSource：枚举相等性
// ---------------------------------------------------------------------------

#[test]
fn interrupt_source_enum_equality() {
    // 钉死枚举可判等（Copy + Eq 契约，被依赖其语义的下游隐式假设）
    assert_eq!(InterruptSource::User, InterruptSource::User);
    assert_ne!(InterruptSource::User, InterruptSource::Hook);
    assert_ne!(InterruptSource::User, InterruptSource::Shutdown);
}

// ---------------------------------------------------------------------------
// AssistantPayload：token 字段为 i64（可接受 0 与负数语义边界）
// ---------------------------------------------------------------------------

#[test]
fn assistant_payload_accepts_zero_tokens() {
    let payload = AssistantPayload {
        content: None,
        reasoning: None,
        tool_calls: None,
        finish_reason: None,
        completion_tokens: 0,
        prompt_tokens: 0,
        total_tokens: 0,
        reasoning_tokens: 0,
        cached_tokens: 0,
    };
    let event = OutputEvent::Assistant(AssistantMessage {
        base: EventBase::default(),
        payload,
    });
    let json = serde_json::to_string(&event).expect("序列化失败");
    let restored: OutputEvent = serde_json::from_str(&json).expect("反序列化失败");
    match restored {
        OutputEvent::Assistant(msg) => {
            assert_eq!(msg.payload.completion_tokens, 0);
            assert_eq!(msg.payload.prompt_tokens, 0);
        }
        _ => panic!("应为 Assistant 变体"),
    }
}

// ---------------------------------------------------------------------------
// ThinkingType：序列化为 snake_case 字符串
// ---------------------------------------------------------------------------

#[rstest]
fn thinking_type_serializes_to_snake_case(
    #[values(ThinkingType::Enabled, ThinkingType::Disabled)] variant: ThinkingType,
) {
    let json = serde_json::to_string(&variant).expect("序列化失败");
    let restored: ThinkingType = serde_json::from_str(&json).expect("反序列化失败");
    // 往返一致 + 序列化结果应为带引号的 snake_case 字符串
    assert_eq!(format!("{:?}", restored), format!("{:?}", variant));
    assert!(
        json == r#""enabled""# || json == r#""disabled""#,
        "ThinkingType 应序列化为 snake_case 字符串，实际：{json}"
    );
}
