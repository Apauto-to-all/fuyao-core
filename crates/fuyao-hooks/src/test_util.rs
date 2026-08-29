//! 钩子测试共享 fixture
//!
//! 构造驱动钩子链路的 OutputEvent 事件与 [`SessionSender`] 通道夹具，
//! 供本 crate 单元测试、集成测试（tests/）与下游 crate（guard 等）的测试共用。
//! 模块以 `#[doc(hidden)] pub` 暴露：集成测试编译 crate 时 `cfg(test)` 模块不存在，
//! 这是单测与集成测试共享同一实现的唯一通道；文档隐藏表明它不属于公开 API 契约。

use fuyao_api::message::QueueEntry;
use fuyao_api::message::output::{
    ChunkMessage, ChunkPayload, InterruptMessage, ToolCallMessage, ToolCallPayload,
    ToolResultMessage, ToolResultPayload,
};
use fuyao_api::message::{EventBase, OutputEvent};
use tokio::sync::mpsc::{Receiver, UnboundedReceiver};

use crate::SessionSender;

/// 构造流式文本块消息（content / reasoning 均可为 None）
pub fn make_chunk(content: Option<&str>, reasoning: Option<&str>) -> ChunkMessage {
    ChunkMessage {
        base: EventBase::default(),
        payload: ChunkPayload {
            content: content.map(|s| s.to_string()),
            reasoning: reasoning.map(|s| s.to_string()),
        },
    }
}

/// 构造空载荷 Chunk 事件（钩子链路占位用，测试不关心载荷内容）
pub fn empty_chunk_event() -> OutputEvent {
    OutputEvent::Chunk(make_chunk(None, None))
}

/// 构造工具调用消息（tool_call_id 固定 call_1，tool_args 解析失败兜底为 Null）
pub fn make_tool_call(name: &str, args: &str) -> ToolCallMessage {
    ToolCallMessage {
        base: EventBase::default(),
        payload: ToolCallPayload {
            tool_call_id: "call_1".to_string(),
            tool_name: name.to_string(),
            tool_args: serde_json::from_str(args).unwrap_or(serde_json::Value::Null),
        },
    }
}

/// 构造工具结果消息（tool_call_id 固定 call_1）
pub fn make_tool_result(name: &str, content: &str) -> ToolResultMessage {
    ToolResultMessage {
        base: EventBase::default(),
        payload: ToolResultPayload {
            tool_call_id: "call_1".to_string(),
            tool_name: name.to_string(),
            content: content.to_string(),
        },
    }
}

/// 构造绑定指定插件名与 session 的 [`SessionSender`] + 三条通道接收端
///
/// 返回 (sender, 统一入站 rx, 中断 rx, 出站事件 rx)，测试按需解构验证消息落点；
/// 有界通道容量 16，出站通道无界，与生产装配的通道形态一致。
pub fn make_sender(
    name: &str,
    session_id: &str,
) -> (
    SessionSender,
    Receiver<QueueEntry>,
    Receiver<InterruptMessage>,
    UnboundedReceiver<OutputEvent>,
) {
    let (tx_inbound, rx_inbound) = tokio::sync::mpsc::channel(16);
    let (tx_interrupt, rx_interrupt) = tokio::sync::mpsc::channel(16);
    let (tx_event, rx_event) = tokio::sync::mpsc::unbounded_channel::<OutputEvent>();
    let sender = SessionSender::new(name, session_id, tx_inbound, tx_interrupt, tx_event);
    (sender, rx_inbound, rx_interrupt, rx_event)
}
