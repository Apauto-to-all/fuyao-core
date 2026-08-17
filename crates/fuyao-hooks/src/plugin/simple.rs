//! 无状态插件快捷构造
//!
//! [`simple_plugin`] 把"一个注册闭包"直接变成 [`Plugin`]：无需定义
//! Plugin struct + PluginInstance struct + 两个 trait impl 的样板。
//!
//! 定位：**无 per-session 状态**的插件（如纯日志、纯统计钩子）走这条快捷路径，
//! 注册闭包被所有 session 共享（每 session 装配时调用一次，注册各自的钩子）；
//! 有状态插件（每 session 需独立计数器、缓存等）仍走完整的
//! Plugin/PluginInstance 两层模型。两条路径共存，按插件特性选择。

use std::sync::Arc;

use crate::HooksRegistry;
use crate::plugin::SessionSender;
use crate::plugin::factory::Plugin;
use crate::plugin::instance::PluginInstance;

/// 注册闭包句柄：session 装配时调用，把钩子注册进该 session 的 registry
///
/// 工厂与各 session 实例共享同一份句柄（无状态闭包跨 session 复用）。
type RegisterFn = Arc<dyn Fn(&mut HooksRegistry, &SessionSender) + Send + Sync>;

/// 无状态插件：持有名字 + 注册闭包（一个闭包即插件）
///
/// 由 [`simple_plugin`] 构造。实现 [`Plugin`] 时每次 `create_instance`
/// 都返回同一个注册闭包的共享句柄——闭包无 per-session 状态，
/// 各 session 装配时各自调用一次完成钩子注册。
pub struct SimplePlugin {
    /// 插件唯一标识（调试、日志、实例 sender 身份绑定用）
    name: String,
    /// 注册闭包：session 装配时调用，把钩子注册进该 session 的 registry
    register: RegisterFn,
}

impl Plugin for SimplePlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn create_instance(&self) -> Box<dyn PluginInstance> {
        Box::new(SimpleInstance {
            register: Arc::clone(&self.register),
        })
    }
}

/// [`SimplePlugin`] 的 session 实例：直接调用共享注册闭包
///
/// 无独立状态，仅持有注册闭包的共享句柄——register 时把闭包
/// 应用到该 session 的 registry 与 sender 上。
struct SimpleInstance {
    /// 共享注册闭包（与工厂持有同一份）
    register: RegisterFn,
}

impl PluginInstance for SimpleInstance {
    fn register(&self, hooks: &mut HooksRegistry, sender: &SessionSender) {
        (self.register)(hooks, sender);
    }
}

/// 无状态插件快捷构造：一个闭包即插件
///
/// `register` 闭包在每个 session 装配时被调用一次，拿到该 session 的
/// [`HooksRegistry`]（可注册任意数量的拦截/观察钩子）与 [`SessionSender`]
/// （可 clone 保存供钩子内发消息）。闭包需 `Send + Sync`（跨 session 共享）。
///
/// 适用场景：无 per-session 状态的插件。需要每 session 独立状态的插件
/// 仍应实现完整的 [`Plugin`] / [`PluginInstance`] 两层模型。
pub fn simple_plugin(
    name: impl Into<String>,
    register: impl Fn(&mut HooksRegistry, &SessionSender) + Send + Sync + 'static,
) -> SimplePlugin {
    SimplePlugin {
        name: name.into(),
        register: Arc::new(register),
    }
}
