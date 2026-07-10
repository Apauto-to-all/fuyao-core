//! Core 模块
//!
//! Engine 消息驱动架构。
//! 子系统：dispatch（消息分发管道）、engine（引擎核心，含 InputDispatcher + TurnExecutor）、llm（LLM 交互）、tool_runner（工具运行）、interrupt（中断处理）。
//!
//! 装配入口（init_engine / 一键 start）已上移至 fuyao-app，core 只负责引擎内核。

mod dispatch;
mod engine;
mod handle;
mod interrupt;
mod llm;
mod tool_runner;

pub use engine::{Engine, SharedHooks, SharedTools};
// SharedAgentCtx 现在定义在 fuyao_api 中，此处重新导出保持兼容
pub use fuyao_api::SharedAgentCtx;
pub use handle::EngineHandle;
