//! Session 上下文模块
//!
//! 架构与 loop_guard 一致：
//! - SessionContext：纯会话管理（消息存储、持久化、会话切换）
//! - SessionHooksState：钩子层内部状态，持有 tx_send 和 agent_ctx
//! - register_session_hooks：注册函数，将两者连接

mod hooks;
mod session_context;

pub use hooks::register_session_hooks;
pub use session_context::SessionContext;
