//! Core 模块
//!
//! Engine 消息驱动架构。
//! 子系统：dispatch（消息分发管道）、engine（引擎核心，含 InputDispatcher + TurnExecutor）、llm（LLM 交互）、tool_runner（工具运行）、interrupt（中断处理）。
//! init：SDK 开箱装配入口（init_engine），从 AgentContext 一气呵成装配出可用的 (Engine, EngineHandle)。

pub mod dispatch;
pub mod engine;
pub mod handle;
pub mod init;
pub mod interrupt;
pub mod llm;
pub mod tool_runner;

pub use engine::{Engine, SharedHooks, SharedTools};
// SharedAgentCtx 现在定义在 fuyao_api 中，此处重新导出保持兼容
pub use fuyao_api::SharedAgentCtx;
pub use handle::EngineHandle;
pub use init::{InitError, init_engine};
