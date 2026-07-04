//! 输出事件定义
//!
//! Engine → UI 的事件类型。
//! 每个事件对应一个枚举变体，数据放在独立文件中管理。
//!
//! - `turn_start`: 轮次开始
//! - `chunk`: 流式输出块（文本/推理片段）
//! - `tool_call`: 工具调用
//! - `tool_result`: 工具结果
//! - `assistant`: 助手消息
//! - `error`: 错误
//! - `plugin`: 插件事件
//! - `queue_update`: 队列状态更新（双队列深度通知）

mod assistant;
mod chunk;
mod error;
mod interrupt;
mod plugin;
mod queue_update;
mod tool_call;
mod tool_result;
mod turn_start;
mod user_message;

pub use assistant::AssistantData;
pub use chunk::ChunkData;
pub use error::ErrorData;
pub use interrupt::InterruptData;
pub use plugin::PluginData;
pub use queue_update::{QueueUpdateData, QueueUpdateKind};
pub use tool_call::ToolCallData;
pub use tool_result::ToolResultData;
pub use turn_start::TurnStartData;
pub use user_message::UserMessageData;

/// 输出事件（Engine → UI）
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
pub enum OutputEvent {
    /// 轮次开始（引擎每次 ReAct 轮次开始时发出）
    TurnStart(TurnStartData),
    /// 流式输出块（文本/推理片段）
    Chunk(ChunkData),
    /// 用户消息（引擎处理用户输入后发出，CLI 据此渲染）
    UserMessage(UserMessageData),
    /// 工具调用
    ToolCall(ToolCallData),
    /// 工具结果
    ToolResult(ToolResultData),
    /// 助手消息
    Assistant(AssistantData),
    /// 中断（引擎中断轮次后发出，CLI 据此渲染中断信息）
    Interrupt(InterruptData),
    /// 错误
    Error(ErrorData),
    /// 插件事件
    Plugin(PluginData),
    /// 队列状态更新（双队列深度变化时发出）
    QueueUpdate(QueueUpdateData),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    #[test]
    fn turn_start_variant() {
        let event = OutputEvent::TurnStart(TurnStartData {
            base: EventBase::default(),
        });
        assert!(matches!(event, OutputEvent::TurnStart(_)));
    }

    #[test]
    fn chunk_variant() {
        let event = OutputEvent::Chunk(ChunkData {
            base: EventBase::default(),
            content: Some("hello".into()),
            reasoning: None,
        });
        assert!(matches!(event, OutputEvent::Chunk(_)));
    }

    #[test]
    fn user_message_variant() {
        let event = OutputEvent::UserMessage(UserMessageData {
            base: EventBase::default(),
            content: "你好".into(),
            mode: crate::message::UserMessageMode::Guide,
            source: crate::message::UserMessageSource::User,
        });
        assert!(matches!(event, OutputEvent::UserMessage(_)));
    }

    #[test]
    fn tool_result_variant() {
        let event = OutputEvent::ToolResult(ToolResultData {
            base: EventBase::default(),
            tool_call_id: "call_1".into(),
            tool_name: "get_weather".into(),
            content: "sunny".into(),
        });
        assert!(matches!(event, OutputEvent::ToolResult(_)));
    }

    #[test]
    fn assistant_variant() {
        let event = OutputEvent::Assistant(AssistantData {
            base: EventBase::default(),
            content: Some("reply".into()),
            reasoning: None,
            tool_calls: None,
            finish_reason: Some("stop".into()),
            completion_tokens: 10,
            prompt_tokens: 20,
            total_tokens: 30,
            reasoning_tokens: 0,
            cached_tokens: 0,
        });
        assert!(matches!(event, OutputEvent::Assistant(_)));
    }

    #[test]
    fn error_variant() {
        let event = OutputEvent::Error(ErrorData {
            base: EventBase::default(),
            message: "oops".into(),
            recoverable: false,
        });
        assert!(matches!(event, OutputEvent::Error(_)));
    }

    #[test]
    fn interrupt_variant() {
        let event = OutputEvent::Interrupt(InterruptData {
            base: EventBase::default(),
            reason: "循环检测".into(),
            source: crate::message::InterruptSource::Hook,
        });
        assert!(matches!(event, OutputEvent::Interrupt(_)));
    }

    #[test]
    fn plugin_variant() {
        let event = OutputEvent::Plugin(PluginData {
            base: EventBase::default(),
            source: "test".into(),
            event_type: "custom".into(),
            data: None,
            error: None,
            message: None,
        });
        assert!(matches!(event, OutputEvent::Plugin(_)));
    }

    #[test]
    fn queue_update_variant() {
        let event = OutputEvent::QueueUpdate(QueueUpdateData {
            base: EventBase::default(),
            guide_count: 2,
            pending_count: 1,
            kind: QueueUpdateKind::Enqueued,
        });
        assert!(matches!(event, OutputEvent::QueueUpdate(_)));
    }

    #[test]
    fn output_event_clone_works() {
        let event = OutputEvent::Chunk(ChunkData {
            base: EventBase::default(),
            content: Some("clone".into()),
            reasoning: None,
        });
        let cloned = event.clone();
        assert!(matches!(cloned, OutputEvent::Chunk(_)));
    }
}
