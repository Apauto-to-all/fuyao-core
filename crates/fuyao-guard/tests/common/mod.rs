//! fuyao-guard 集成测试共享 fixture
//!
//! 构造 OutputEvent 事件用于驱动 LoopGuardPlugin 的钩子链路。
//! 这些构造器复制自 guard.rs 源码内单元测试（该模块的 make_* 为私有），
//! 仅用于集成测试侧的端到端装配验证。

// 跨测试二进制共享：未用部分不报 dead_code
#![allow(dead_code)]

use fuyao_api::message::EventBase;
use fuyao_api::message::output::{
    ChunkMessage, ChunkPayload, ToolCallMessage, ToolCallPayload, ToolResultMessage,
    ToolResultPayload,
};

/// 构造流式文本块事件
pub fn make_chunk(content: Option<&str>, reasoning: Option<&str>) -> ChunkMessage {
    ChunkMessage {
        base: EventBase::default(),
        payload: ChunkPayload {
            content: content.map(|s| s.to_string()),
            reasoning: reasoning.map(|s| s.to_string()),
        },
    }
}

/// 构造工具调用事件
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

/// 构造工具结果事件
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
