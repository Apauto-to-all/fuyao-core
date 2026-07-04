//! 消息模块
//!
//! 定义输入事件类型（UI → Engine）。
//! 定义输出事件类型（Engine → UI）。

pub mod event_base;
pub mod event_input;
pub mod event_output;

pub use event_base::EventBase;
pub use event_input::{
    InputEvent, InterruptData, InterruptSource, PluginSource, SystemSource, UserData,
    UserMessageMode, UserMessageSource,
};
pub use event_output::{
    AssistantData, ChunkData, ErrorData, InterruptData as OutputInterruptData, OutputEvent,
    PluginData, QueueUpdateData, QueueUpdateKind, ToolCallData, ToolResultData, TurnStartData,
    UserMessageData,
};
