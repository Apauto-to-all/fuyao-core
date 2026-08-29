//! 钩子系统
//!
//! 拦截钩子（同步串行，原地修改事件，可阻止带原因，panic 防护）+
//! 观察钩子（异步串行，Arc 共享只读，panic 防护 + 超时防护）。
//! 两类钩子统一按优先级排序执行（priority 降序、同优先级按注册序，
//! finalize 装配期一次排定）。
//!
//! 系统定位：钩子与插件面向**二次开发者**（基于本 SDK 构建应用的 Rust 开发者），
//! 是引擎对外的公开扩展契约——API 完备性按外部使用价值评判，不按内置插件的
//! 使用程度评判。内置插件只是参考实现，未用满全部能力属预期，不构成删减依据。
//!
//! Void/Modifying 分离模式：
//! - 拦截钩子（Intercept）：串行执行，原地修改数据或阻止操作
//! - 观察钩子（Observe）：串行执行，只读副作用（日志、持久化、统计），
//!   多钩子共享同一份只读事件（Arc）
//!
//! 插件两层模型：
//! - [`Plugin`]（工厂模板）：引擎级，构造时持配置；每 session 调用 `create_instance` 生成实例
//! - [`PluginInstance`]（session 实例）：session 级，持 per-session 独立状态，
//!   register 时注册 hook 并接收该 session 的 [`SessionSender`]
//! - [`simple_plugin`]：无状态插件快捷构造，一个闭包即插件（省去两层样板），
//!   与完整两层模型共存
//!
//! 注册表装配后冻结：register 只发生在 session 装配期，finalize 排定后以
//! `Arc<HooksRegistry>` 只读共享（运行期无锁，hook 持共享状态需自行内部同步）。

use std::sync::Arc;

mod plugin;
mod registry;
mod types;

pub use plugin::{
    NamedPluginInstance, Plugin, PluginHost, PluginInstallError, PluginInstance, SessionSender,
    SimplePlugin, panic_payload_to_string, simple_plugin,
};
pub use registry::HooksRegistry;
pub use types::{OutputInterceptFn, OutputObserveFn};

/// 测试共享 fixture（事件构造器 + SessionSender 通道夹具）
///
/// 面向单元测试、集成测试与下游 crate 的测试；doc(hidden) 表明不属于公开 API 契约。
#[doc(hidden)]
pub mod test_util;

/// 共享钩子注册表
///
/// 引擎在每个 session 装配时新建一份（装配期注册 + finalize 冻结），
/// 运行期以 `Arc` 只读共享给该 session 的所有 dispatch 调用点——无锁。
/// 定义在 fuyao-hooks 是为了让 Plugin trait 等签名能引用它，而不产生对 fuyao-core 的环依赖。
pub type SharedHooks = Arc<HooksRegistry>;
