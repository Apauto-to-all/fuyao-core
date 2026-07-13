//! fuyao-core 引擎内核
//!
//! 能力共享层：启动一次，装配能力（provider / DB 句柄 / 出口通道）；
//! 多个对话按需创建，各自独立跑交互。
//!
//! 公开 API 遵循设计文档的四个动作：
//! - 启动引擎（[`Engine::new`]）
//! - 创建对话（[`Engine::create_session`]）
//! - 恢复对话（[`Engine::resume_session`]）
//! - 入事件（[`Engine::send`]，单一入口，对话级事件）
//! - 出事件（[`Engine::recv`]，单一出口，出所有 OutputEvent）
//! - 关闭引擎（[`Engine::shutdown`]，独立方法，不走消息流）
//!
//! 当前为骨架阶段，四个动作的签名已定，内部逻辑后续逐步填充。

mod engine;
mod error;

pub use engine::{Engine, SessionId};
pub use error::EngineError;
