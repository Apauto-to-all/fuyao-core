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

mod dispatch;
mod emit;
mod engine;
mod error;
mod interrupt;
mod react;
mod stream;
mod tool_exec;
mod tool_registry;

pub use engine::{Engine, SessionId};
pub use error::EngineError;
pub use tool_registry::{ToolEntry, ToolRegistry, ToolRegistryBuilder};

/// 插件相关类型的便捷重导出
///
/// 装配方从 `fuyao_core` 一处拿插件相关类型（无需直接依赖 `fuyao-hooks`）：
/// - [`PluginHost`]：构造空 host → `add` 注册插件工厂 → 传入 [`Engine::new`]
/// - [`Plugin`] / [`PluginInstance`]：实现自定义插件
/// - [`SessionSender`]：插件通过 send_input hook 拿到，用于发消息
///
/// 引擎内部 per-session 装配时调 [`PluginHost::create_instances`] 生成实例，
/// 各实例 register 到该 session 私有的 HooksRegistry。
pub use fuyao_hooks::{Plugin, PluginHost, PluginInstance, SessionSender, SharedHooks};
