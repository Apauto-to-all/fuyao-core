//! 输出事件定义
//!
//! Engine → UI 的事件类型。
//! 每个事件对应一个枚举变体，envelope + payload 放在独立文件中管理。
//!
//! - `chunk`: 流式输出块（文本/推理片段）
//! - `user`: 用户消息（引擎处理用户输入后发出）
//! - `tool_call`: 工具调用
//! - `tool_result`: 工具结果
//! - `assistant`: 助手消息
//! - `interrupt`: 中断（引擎中断轮次后发出）
//! - `error`: 错误
//! - `plugin`: 插件事件

mod assistant;
mod chunk;
mod error;
mod interrupt;
mod plugin;
mod tool_call;
mod tool_result;
mod user_message;

// envelope / payload 在 output 层导出（外部通过 output::UserMessage 等路径访问）
pub use assistant::{AssistantMessage, AssistantPayload};
pub use chunk::{ChunkMessage, ChunkPayload};
pub use error::{ErrorMessage, ErrorPayload};
pub use interrupt::{InterruptMessage, InterruptPayload};
pub use plugin::{PluginMessage, PluginPayload};
pub use tool_call::{ToolCallMessage, ToolCallPayload};
pub use tool_result::{ToolResultMessage, ToolResultPayload};
pub use user_message::{UserMessage, UserPayload};

/// 输出事件（Engine → UI）
///
/// envelope 每变体自带 base（在 envelope struct 内），enum 保持穷尽匹配。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum OutputEvent {
    /// 流式输出块（文本/推理片段）
    Chunk(ChunkMessage),
    /// 用户消息（引擎处理用户输入后发出，CLI 据此渲染）
    User(UserMessage),
    /// 工具调用
    ToolCall(ToolCallMessage),
    /// 工具结果
    ToolResult(ToolResultMessage),
    /// 助手消息
    Assistant(AssistantMessage),
    /// 中断（引擎中断轮次后发出，CLI 据此渲染中断信息）
    Interrupt(InterruptMessage),
    /// 错误
    Error(ErrorMessage),
    /// 插件事件
    Plugin(PluginMessage),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;
    use crate::message::input::{InterruptSource, PluginEventSource};

    #[test]
    fn chunk_variant() {
        let event = OutputEvent::Chunk(ChunkMessage {
            base: EventBase::default(),
            payload: ChunkPayload {
                content: Some("hello".into()),
                reasoning: None,
            },
        });
        assert!(matches!(event, OutputEvent::Chunk(_)));
    }

    #[test]
    fn user_variant() {
        let event = OutputEvent::User(UserMessage {
            base: EventBase::default(),
            payload: UserPayload {
                content: "你好".into(),
                mode: crate::message::UserMessageMode::Guide,
                source: crate::message::UserMessageSource::User,
            },
        });
        assert!(matches!(event, OutputEvent::User(_)));
    }

    #[test]
    fn tool_result_variant() {
        let event = OutputEvent::ToolResult(ToolResultMessage {
            base: EventBase::default(),
            payload: ToolResultPayload {
                tool_call_id: "call_1".into(),
                tool_name: "get_weather".into(),
                content: "sunny".into(),
            },
        });
        assert!(matches!(event, OutputEvent::ToolResult(_)));
    }

    #[test]
    fn assistant_variant() {
        let event = OutputEvent::Assistant(AssistantMessage {
            base: EventBase::default(),
            payload: AssistantPayload {
                content: Some("reply".into()),
                reasoning: None,
                tool_calls: None,
                finish_reason: Some("stop".into()),
                completion_tokens: 10,
                prompt_tokens: 20,
                total_tokens: 30,
                reasoning_tokens: 0,
                cached_tokens: 0,
            },
        });
        assert!(matches!(event, OutputEvent::Assistant(_)));
    }

    #[test]
    fn error_variant() {
        let event = OutputEvent::Error(ErrorMessage {
            base: EventBase::default(),
            payload: ErrorPayload {
                message: "oops".into(),
                recoverable: false,
            },
        });
        assert!(matches!(event, OutputEvent::Error(_)));
    }

    #[test]
    fn interrupt_variant() {
        let event = OutputEvent::Interrupt(InterruptMessage {
            base: EventBase::default(),
            payload: InterruptPayload {
                reason: "循环检测".into(),
                source: InterruptSource::Hook,
            },
        });
        assert!(matches!(event, OutputEvent::Interrupt(_)));
    }

    #[test]
    fn plugin_variant() {
        let event = OutputEvent::Plugin(PluginMessage {
            base: EventBase::default(),
            payload: PluginPayload {
                source: PluginEventSource {
                    name: "test".into(),
                },
                event_type: "custom".into(),
                data: None,
                error: None,
                message: None,
            },
        });
        assert!(matches!(event, OutputEvent::Plugin(_)));
    }

    #[test]
    fn output_event_clone_works() {
        let event = OutputEvent::Chunk(ChunkMessage {
            base: EventBase::default(),
            payload: ChunkPayload {
                content: Some("clone".into()),
                reasoning: None,
            },
        });
        let cloned = event.clone();
        assert!(matches!(cloned, OutputEvent::Chunk(_)));
    }
}
