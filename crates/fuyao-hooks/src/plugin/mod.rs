//! 插件抽象
//!
//! 两层模型：[`Plugin`]（引擎级工厂模板）+ [`PluginInstance`]（session 级实例）。
//!
//! - 引擎启动时装配 `Arc<PluginHost>`（持所有 [`Plugin`]）
//! - 每个 session 装配时调用 [`PluginHost::create_instances`] 生成实例 Vec
//! - 调用方（引擎）逐个 `instance.register(&mut hooks)` 注册到该 session 的 registry
//! - 通过 send_input hook 把 [`SessionSender`] 传给插件（统一原则：发消息能力走 hook）
//!
//! 文件夹模块拆分（避免单文件过大，按职责命名）：
//! - [`factory`](crate::plugin::factory): Plugin trait（引擎级工厂模板）
//! - [`instance`](crate::plugin::instance): PluginInstance trait（session 级实例）
//! - [`sender`](crate::plugin::sender): SessionSender（封装三通道分流）
//! - [`host`](crate::plugin::host): PluginHost + PluginInstallError + panic 辅助

mod factory;
mod host;
mod instance;
mod sender;
#[cfg(test)]
mod tests;

pub use factory::Plugin;
pub use host::{PluginHost, PluginInstallError};
pub use instance::PluginInstance;
pub use sender::SessionSender;

pub(crate) use host::panic_payload_to_string;
