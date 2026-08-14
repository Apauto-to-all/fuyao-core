//! PluginInstance trait —— session 级插件实例
//!
//! 每个插件工厂（[`Plugin`](crate::Plugin)）在每个 session 装配时调用
//! [`Plugin::create_instance`](crate::Plugin::create_instance) 生成一个实例。
//! 实例持有该 session 独立的状态（有状态插件），或仅 clone 共享依赖（无状态插件）。
//!
//! register 方法是**同步**的：它只把 hook 闭包注册到 HooksRegistry 并保存
//! 发送器，不执行任何异步操作。注册的闭包内部可以是 async 的（执行时被 await）。
//!
//! 发消息能力经 register 的 `sender` 参数直接获得：需要发消息的插件在此 clone
//! 保存 [`SessionSender`](crate::SessionSender)，不需要的可忽略。

use crate::HooksRegistry;
use crate::plugin::SessionSender;

/// PluginInstance —— session 级插件实例
///
/// 由 [`Plugin::create_instance`](crate::Plugin::create_instance) 生成，每 session 一个。
/// 通过 [`register`](Self::register) 把闭包注册到该 session 的 [`HooksRegistry`]，
/// 同时接收绑定该插件身份的 [`SessionSender`]。
///
/// 实现自由度：
/// - 字段：完全自由（持 per-session state、共享 Arc、配置、无字段都合法）
/// - 注册几个 hook：完全自由（0 个、1 个、2 个都合法）
/// - 是否要发消息：完全自由（忽略 sender 就只能观察/拦截）
pub trait PluginInstance: Send + Sync {
    /// 注册阶段：把 hook 闭包注册到 registry，并接收该 session 的消息发送器
    ///
    /// 引擎在每个 session 装配时，新建一个空的 HooksRegistry，调用此方法让插件
    /// 把它的 hook 注册进去（注册顺序 = 同优先级下的执行顺序），同时传入绑定
    /// 该插件名的 [`SessionSender`]——需要发消息的插件在此 clone 保存。
    ///
    /// **同步**：只做注册与保存动作（push 闭包到 Vec），不执行异步操作。
    fn register(&self, hooks: &mut HooksRegistry, sender: &SessionSender);

    /// 销毁阶段：引擎卸载该 session 时调用，用于清理资源
    ///
    /// 默认空实现。需要清理的插件（如关闭连接、刷盘）自行覆盖。
    /// 单个实例 dispose panic 不阻塞其他实例（由 PluginHost 防护）。
    fn dispose(&self) {}
}
