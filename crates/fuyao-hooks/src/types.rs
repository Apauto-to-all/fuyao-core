//! 钩子类型定义
//!
//! 拦截结果 + 钩子函数签名类型别名。

use crate::plugin::SessionSender;
use fuyao_api::message::OutputEvent;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// 拦截钩子返回值：通过或阻止
///
/// 拦截只负责修改或阻止事件，不承载中断职责。
/// 中断统一通过 [`SessionSender::send_interrupt`](crate::SessionSender::send_interrupt) 发送，
/// 由引擎主循环处理。
#[derive(Debug)]
pub enum InterceptResult<T> {
    /// 通过，携带值（可能被修改）
    Pass(T),
    /// 阻止当前事件，携带原因
    Block(String),
}

/// 输出拦截钩子：可修改或阻止事件
pub type OutputInterceptFn =
    Arc<dyn Fn(&OutputEvent) -> InterceptResult<OutputEvent> + Send + Sync>;

/// 输出观察钩子：异步副作用
pub type OutputObserveFn =
    Arc<dyn Fn(OutputEvent) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// 发送输入事件钩子：插件获取 [`SessionSender`] 的唯一入口
///
/// 引擎在每个 session 装配时调用一次此 hook，传入绑定该 session 的 [`SessionSender`]。
/// 插件保存此 sender，后续在任何时机（不依赖 emit 频率）调用其方法发送消息：
/// - [`send_user`](SessionSender::send_user) / [`send_user_with_mode`](SessionSender::send_user_with_mode)
/// - [`send_interrupt`](SessionSender::send_interrupt)
/// - [`send_plugin`](SessionSender::send_plugin) / [`send_plugin_data`](SessionSender::send_plugin_data) /
///   [`send_plugin_full`](SessionSender::send_plugin_full)
///
/// **统一原则**：发消息能力只通过此 hook 获得。没有 hook 之外的"特殊注入通道"。
/// 插件不注册 send_input hook 就不能主动发消息（但仍可观察/拦截）。
///
/// SessionSender 绑定的是该 session 的三条通道（不是全局 tx），
/// 多 session 并发时各 session 的 sender 完全隔离。
pub type SendInputFn =
    Arc<dyn Fn(SessionSender) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
