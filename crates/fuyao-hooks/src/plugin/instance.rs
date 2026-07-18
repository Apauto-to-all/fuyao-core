//! PluginInstance trait —— session 级插件实例
//!
//! 每个插件工厂（[`Plugin`](crate::Plugin)）在每个 session 装配时调用
//! [`Plugin::create_instance`](crate::Plugin::create_instance) 生成一个实例。
//! 实例持有该 session 独立的状态（有状态插件），或仅 clone 共享依赖（无状态插件）。
//!
//! register 方法是**同步**的：它只是把 hook 闭包注册到 HooksRegistry，
//! 不执行任何异步操作。注册的闭包内部可以是 async 的（执行时被 await）。
//!
//! 发消息能力通过 send_input hook 获得：register 时注册 send_input hook，
//! 引擎在装配该 session 后调用该 hook 传入 [`SessionSender`](crate::SessionSender)，
//! 插件在 hook 回调里保存 sender。

use crate::HooksRegistry;

/// PluginInstance —— session 级插件实例
///
/// 由 [`Plugin::create_instance`](crate::Plugin::create_instance) 生成，每 session 一个。
/// 通过 [`register`](Self::register) 把闭包注册到该 session 的 [`HooksRegistry`]。
///
/// **不接收 sender 参数**：发消息能力只通过 send_input hook 获得。
/// 这是"统一原则"——插件的一切能力都是 hook，没有 hook 之外的注入通道。
///
/// 实现自由度：
/// - 字段：完全自由（持 per-session state、共享 Arc、配置、无字段都合法）
/// - 注册几个 hook：完全自由（0 个、1 个、2 个、3 个都合法）
/// - 是否要发消息：完全自由（不注册 send_input 就只能观察/拦截）
pub trait PluginInstance: Send + Sync {
    /// 注册阶段：把 hook 闭包注册到 registry
    ///
    /// 引擎在每个 session 装配时，新建一个空的 HooksRegistry，调用此方法让插件
    /// 把它的 hook 注册进去。注册顺序 = 执行顺序。
    ///
    /// **同步**：只做注册动作（push 闭包到 Vec），不执行异步操作。
    fn register(&self, hooks: &mut HooksRegistry);

    /// 销毁阶段：引擎卸载该 session 时调用，用于清理资源
    ///
    /// 默认空实现。需要清理的插件（如关闭连接、刷盘）自行覆盖。
    /// 单个实例 dispose panic 不阻塞其他实例（由 PluginHost 防护）。
    fn dispose(&self) {}
}
