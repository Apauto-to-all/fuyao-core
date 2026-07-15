//! 钩子系统
//!
//! 拦截钩子（异步串行，可取消带原因，panic 防护）+ 观察钩子（异步串行）。
//! 按优先级排序执行。
//!
//! Void/Modifying 分离模式：
//! - 拦截钩子（Intercept）：串行执行，可修改数据或阻止操作
//! - 观察钩子（Observe）：串行执行，只读副作用（日志、持久化、统计）

use std::sync::Arc;

mod plugin;
mod registry;
mod types;

pub use plugin::{Plugin, PluginEmitter, PluginHost, PluginInstallError};
pub use registry::HooksRegistry;
pub use types::{
    BeforeLlmFn, BeforeLlmOutput, InterceptResult, LlmErrorAction, OnLlmErrorFn, OutputInterceptFn,
    OutputObserveFn, SendInputFn,
};

/// 共享钩子注册表
///
/// fuyao-core 的 EngineHandle / dispatcher / emitter 通过此类型持有钩子注册表。
/// 定义在 fuyao-hooks 是为了让 Plugin trait 签名能引用它，而不产生对 fuyao-core 的环依赖。
pub type SharedHooks = Arc<tokio::sync::Mutex<HooksRegistry>>;
