//! fuyao-core 引擎内核
//!
//! 能力共享层：启动一次，装配能力（provider / DB / 工具 / 插件）；
//! 多个对话按需创建，各自独立跑交互。
//!
//! 公开 API：
//! - 启动引擎（[`Engine::new`]）
//! - 创建对话（[`Engine::create_session`]，返 `(id, rx)`——rx 是 per-session 出站通道）
//! - 恢复对话（[`Engine::resume_session`]）
//! - 入事件（[`Engine::send`]，单一入口，对话级事件）
//! - 关闭引擎（[`Engine::shutdown`]，独立方法，不走消息流）
//!
//! **没有 Engine::recv**——出站靠每 session 自己的 rx 消费（per-session 通道化）。
//! 装配层（fuyao-app）负责 fan-in 多个 session 的 rx 为单一出口。

mod dispatch;
mod emit;
mod engine;
mod error;
mod history;
mod interrupt;
mod react;
mod stream;
mod tool_exec;
mod tool_registry;

pub use engine::{Engine, SessionId};
// ChildSessionSource 由 fuyao-api 定义并导出；这里重导出让上层从 fuyao-core 一处拿
pub use error::EngineError;
// 历史回放投影：存储 Message → OutputEvent 的对外唯一出口。上层查询接口用它把
// 会话历史投影成与实时流同构的事件；映射细节（含 tool_calls 嵌套解析）归 history 模块内化
pub use fuyao_api::ChildSessionSource;
pub use history::messages_to_events;
pub use tool_registry::{ToolRegistry, ToolRegistryBuilder};

/// 插件相关类型的便捷重导出
///
/// 装配方从 `fuyao_core` 一处拿插件相关类型（无需直接依赖 `fuyao-hooks`）：
/// - [`PluginHost`]：构造空 host → `add` 注册插件工厂 → 传入 [`Engine::new`]
/// - [`Plugin`] / [`PluginInstance`]：实现自定义插件
/// - [`SessionSender`]：插件经 register 参数拿到，用于发消息
///
/// 引擎内部 per-session 装配时调 [`PluginHost::create_instances`] 生成 `(名, 实例)` 配对，
/// 各实例 register 到该 session 私有的 HooksRegistry（sender 绑该插件名）。
pub use fuyao_hooks::{Plugin, PluginHost, PluginInstance, SessionSender, SharedHooks};
