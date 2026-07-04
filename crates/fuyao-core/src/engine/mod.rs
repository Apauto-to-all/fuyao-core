//! 引擎 - 消息驱动的 Agent 核心
//!
//! 架构：InputDispatcher + TurnExecutor 双并发 actor
//! - InputDispatcher（Engine）：始终响应输入消息，按模式入队 + 通知
//! - TurnExecutor：队列驱动，消费 guide_queue 执行 ReAct 循环
//! - EngineHandle：UI 层句柄，发送输入事件、接收输出事件

mod dispatcher;
mod emitter;
mod turn_executor;
pub(crate) mod types;

// 对外公共 API
pub use dispatcher::Engine;
pub use types::{SharedHooks, SharedTools};
// 仅 crate 内部使用
pub(crate) use emitter::EventEmitter;
