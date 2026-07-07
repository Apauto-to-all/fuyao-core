//! 消息模块
//!
//! 定义输入事件类型（UI → Engine）。
//! 定义输出事件类型（Engine → UI）。

pub mod event_base;
pub mod input;
pub mod output;

pub use event_base::EventBase;
// 共享枚举/结构（无方向语义，输入输出共享）导出到 message 根
pub use input::{
    InterruptSource, PluginEventSource, PluginSource, SystemSource, UserMessageMode,
    UserMessageSource,
};
// 事件 enum 导出
pub use input::InputEvent;
pub use output::{OutputEvent, QueueUpdateKind};
// 注：envelope / payload 不在此导出，外部通过 input::UserMessage / output::UserMessage 等路径访问
// （输入输出 envelope/payload 同名，靠模块路径区分，避免根导出撞名）
