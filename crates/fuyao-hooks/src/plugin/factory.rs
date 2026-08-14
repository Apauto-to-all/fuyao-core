//! Plugin trait —— 引擎级插件工厂模板
//!
//! 工厂在每个 Engine 启动时装配一次（持配置/共享依赖），通过
//! [`create_instance`](Plugin::create_instance) 在每 session 装配时生成独立实例。
//!
//! 两层模型（Plugin 工厂 + PluginInstance 实例）的生命周期：
//! - **Plugin（工厂）**：引擎级，构造时持配置/共享依赖（如 LoopGuardConfig）。
//!   全引擎每个插件只有一份。
//! - **PluginInstance（实例）**：session 级，由 create_instance 生成。
//!   每 session 每插件一份，持有 per-session 独立状态（如 LoopGuard 的循环计数器）。
//!
//! Rust 不支持继承，trait 是接口约定（类似 Java interface），不是基类。
//! 插件作者完全自由设计 struct 内部、自由选择注册几个 hook、自由决定是否持状态。

use crate::plugin::PluginInstance;

/// Plugin —— 引擎级插件工厂
///
/// 全引擎每个插件一份，构造时持有配置和共享依赖。
/// 引擎在每个 session 装配时调用 [`create_instance`](Self::create_instance) 生成
/// 该 session 的独立实例（per-session state 在实例里）。
///
/// **与 [`PluginInstance`] 的区别**：
/// - Plugin（工厂）：生命周期 = Engine，数量 = 每插件 1 份，持配置
/// - PluginInstance（实例）：生命周期 = Session，数量 = 每 session 每插件 1 份，持 state
///
/// 实现示例见模块文档。
pub trait Plugin: Send + Sync {
    /// 插件唯一标识（调试、日志、配置开关匹配、实例 sender 身份绑定用）
    ///
    /// **必须全局唯一**：PluginHost 装配时会校验重名，重名硬失败。
    /// session 装配时以此为名构造该插件实例专属的
    /// [`SessionSender`](crate::SessionSender)（注入消息的 source 据此可追溯）。
    fn name(&self) -> &str;

    /// 工厂方法：生成该 session 的独立实例
    ///
    /// 引擎在每个 session 装配时调用此方法。实现者根据插件特性选择：
    /// - **无状态插件**：返回无字段实例或仅 clone Arc 共享依赖（轻量）
    /// - **有状态插件**：每 session 新建独立 state（如 `Arc<Mutex<LoopGuardState>>`）
    fn create_instance(&self) -> Box<dyn PluginInstance>;

    /// 销毁阶段：引擎卸载时调用，用于清理工厂级资源
    ///
    /// 默认空实现。需要清理的插件（如关闭共享连接）自行覆盖。
    /// 单个 Plugin dispose panic 不阻塞其他插件（由 PluginHost 防护）。
    fn dispose(&self) {}
}
