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
//! - `notice`: 插件通知（插件主动发送的提示，纯实时不落库、不进插件扩展面）
//! - `compression`: 上下文压缩事件（Started/Delta/Ended 三阶段）
//! - `title`: 会话标题更新（首轮对话后异步生成）
//! - `retry`: LLM 重试事件（重试前发，前端据此渲染「N 秒后重试」提示）
//! - `child_session`: 子任务 session 生命周期（子代理 / 后台任务派生时发出）
//! - `control`: 控制命令消息（消费时刻回显：引擎先发出本事件、后执行命令本体）
//!
//! `ControlMessage` 同时也是 guide / pending 双队列的条目载荷：队列中的条目
//! 在消费时机以 `control` 事件回显，此后命令本体才被执行。

mod assistant;
mod child_session;
mod chunk;
mod compression;
mod control;
mod error;
mod interrupt;
mod notice;
mod retry;
mod title;
mod tool_call;
mod tool_result;
mod user_message;

// envelope / payload 在 output 层导出（外部通过 output::UserMessage 等路径访问）
pub use assistant::{AssistantMessage, AssistantPayload};
pub use child_session::{
    ChildSessionMessage, ChildSessionOrigin, ChildSessionPayload, ChildSessionState,
};
pub use chunk::{ChunkMessage, ChunkPayload};
pub use compression::{
    CompressionDeltaPayload, CompressionEndedPayload, CompressionFailedPayload, CompressionMessage,
    CompressionPayload, CompressionReason, CompressionStartedPayload,
};
pub use control::{ControlMessage, ControlPayload};
pub use error::{ErrorMessage, ErrorPayload};
pub use interrupt::{InterruptMessage, InterruptPayload};
pub use notice::{NoticeLevel, PluginNoticeMessage, PluginNoticePayload};
pub use retry::{RetryMessage, RetryPayload};
pub use title::{TitleMessage, TitlePayload};
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
    /// 控制命令消息
    Control(ControlMessage),
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
    /// 插件通知（插件主动发送的提示；纯实时事件，不落库、不经插件拦截/观察面）
    PluginNotice(PluginNoticeMessage),
    /// 上下文压缩事件（含 Started/Delta/Ended 三阶段，前端据此追踪压缩生命周期）
    Compression(CompressionMessage),
    /// 会话标题更新（首轮对话后异步生成，前端据此更新会话列表标题）
    Title(TitleMessage),
    /// LLM 重试事件（重试前发，前端据此渲染「重试中…N 秒后重试，错误：xxx」提示）
    Retry(RetryMessage),
    /// 子任务 session 生命周期（子代理 / 后台任务派生时发，前端据此追踪 child_session_id
    /// 并把后续 session_id == child_session_id 的事件归到此任务的渲染区）
    ChildSession(ChildSessionMessage),
}

impl OutputEvent {
    /// 可变借用事件的 base（落库后回填 seq 用）
    ///
    /// 进历史事件经 `emit_to_history` 落库后，seq 已回填到 [`crate::Message`]，
    /// 调用方据此把 seq 写进事件 base，使实时事件与历史回放同构。
    pub fn base_mut(&mut self) -> &mut crate::message::EventBase {
        match self {
            OutputEvent::Chunk(m) => &mut m.base,
            OutputEvent::User(m) => &mut m.base,
            OutputEvent::Control(m) => &mut m.base,
            OutputEvent::ToolCall(m) => &mut m.base,
            OutputEvent::ToolResult(m) => &mut m.base,
            OutputEvent::Assistant(m) => &mut m.base,
            OutputEvent::Interrupt(m) => &mut m.base,
            OutputEvent::Error(m) => &mut m.base,
            OutputEvent::PluginNotice(m) => &mut m.base,
            OutputEvent::Compression(m) => &mut m.base,
            OutputEvent::Title(m) => &mut m.base,
            OutputEvent::Retry(m) => &mut m.base,
            OutputEvent::ChildSession(m) => &mut m.base,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;
    use crate::message::input::InterruptSource;

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
                images: vec![],
                mode: crate::message::UserMessageMode::Guide,
                source: crate::message::UserMessageSource::User,
                client_message_id: None,
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
    fn compression_variant() {
        let event = OutputEvent::Compression(CompressionMessage {
            base: EventBase::default(),
            payload: CompressionPayload::Started(CompressionStartedPayload {
                reason: CompressionReason::Auto,
            }),
        });
        assert!(matches!(event, OutputEvent::Compression(_)));
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

    #[test]
    fn title_variant() {
        let event = OutputEvent::Title(TitleMessage {
            base: EventBase::default(),
            payload: TitlePayload {
                title: "Rust 异步讨论".into(),
            },
        });
        assert!(matches!(event, OutputEvent::Title(_)));
    }

    #[test]
    fn retry_variant() {
        let event = OutputEvent::Retry(RetryMessage {
            base: EventBase::default(),
            payload: RetryPayload {
                attempt: 2,
                max_retries: 5,
                wait_ms: 4000,
                cause: "速率限制".into(),
            },
        });
        assert!(matches!(event, OutputEvent::Retry(_)));
    }
}
