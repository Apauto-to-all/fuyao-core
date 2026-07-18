//! 钩子系统
//!
//! 拦截钩子（同步串行，可取消带原因，panic 防护）+ 观察钩子（异步串行）。
//! 按优先级排序执行。
//!
//! Void/Modifying 分离模式：
//! - 拦截钩子（Intercept）：串行执行，可修改数据或阻止操作
//! - 观察钩子（Observe）：串行执行，只读副作用（日志、持久化、统计）
//!
//! 插件两层模型：
//! - [`Plugin`]（工厂模板）：引擎级，构造时持配置；每 session 调用 `create_instance` 生成实例
//! - [`PluginInstance`]（session 实例）：session 级，持 per-session 独立状态
//!
//! 发消息能力通过 send_input hook 获得（统一原则：插件一切能力都是 hook）。
//! [`SessionSender`] 封装三通道分流（User/Interrupt/Plugin），每 session 装配时构造一份。

use std::sync::Arc;

mod plugin;
mod registry;
mod types;

pub use plugin::{
    Plugin, PluginHost, PluginInstallError, PluginInstance, SessionSender, panic_payload_to_string,
};
pub use registry::HooksRegistry;
pub use types::{InterceptResult, OutputInterceptFn, OutputObserveFn, SendInputFn};

/// 共享钩子注册表
///
/// 引擎在每个 session 装配时新建一份（per-session 独立），通过 dispatch 管道使用。
/// 定义在 fuyao-hooks 是为了让 Plugin trait 等签名能引用它，而不产生对 fuyao-core 的环依赖。
pub type SharedHooks = Arc<tokio::sync::Mutex<HooksRegistry>>;
