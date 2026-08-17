//! 插件抽象
//!
//! 两层模型：[`Plugin`]（引擎级工厂模板）+ [`PluginInstance`](PluginInstance)（session 级实例）。
//!
//! - 引擎启动时装配 `Arc<PluginHost>`（持所有 [`Plugin`]）
//! - 每个 session 装配时调用 [`PluginHost::create_instances`] 生成 `(插件名, 实例)` 配对 Vec
//! - 调用方（引擎）逐个 `instance.register(&mut hooks, &sender)` 注册到该 session 的
//!   registry，sender 绑定该插件名（注入消息 source 可追溯）
//!
//! 文件夹模块拆分（避免单文件过大，按职责命名）：
//! - [`factory`](crate::plugin::factory): Plugin trait（引擎级工厂模板）
//! - [`instance`](crate::plugin::instance): PluginInstance trait（session 级实例）
//! - [`sender`](crate::plugin::sender): SessionSender（封装两通道分流）
//! - [`host`](crate::plugin::host): PluginHost + PluginInstallError + panic 辅助
//! - [`simple`](crate::plugin::simple): simple_plugin 快捷构造 + SimplePlugin（无状态插件）
//!
//! 无状态插件可走 [`simple_plugin`](crate::plugin::simple::simple_plugin) 快捷路径
//! （一个闭包即插件）；有状态插件走完整两层模型，两条路径共存。

mod factory;
mod host;
mod instance;
mod sender;
mod simple;
#[cfg(test)]
mod tests;

pub use factory::Plugin;
pub use host::{NamedPluginInstance, PluginHost, PluginInstallError, panic_payload_to_string};
pub use instance::PluginInstance;
pub use sender::SessionSender;
pub use simple::{SimplePlugin, simple_plugin};
