//! 钩子系统
//!
//! 拦截钩子（异步串行，可取消带原因，panic 防护）+ 观察钩子（异步串行）。
//! 按优先级排序执行。
//!
//! 设计参考 zeroclaw 的 Void/Modifying 分离模式：
//! - 拦截钩子（Intercept）：串行执行，可修改数据或阻止操作
//! - 观察钩子（Observe）：串行执行，只读副作用（日志、持久化、统计）

mod registry;
mod types;

pub use registry::HooksRegistry;
pub use types::{
    BeforeLlmFn, BeforeLlmOutput, InterceptResult, LlmErrorAction, OnLlmErrorFn, OutputInterceptFn,
    OutputObserveFn, SendInputFn,
};
