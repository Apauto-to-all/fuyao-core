//! 引擎装配流水线：[`crate::start`] 的内部子步骤
//!
//! 从配置 / 日志 / Provider 准备到工具收集的装配过程，由 [`crate::start`]
//! 按序编排。各模块不单独对外，产物经装配产物 [`crate::FuyaoApp`] 暴露。

mod init;
mod logging;
mod mcp;
mod tools;

pub use init::{InitError, InitResult, init_engine};
pub use logging::LogGuard;
pub(crate) use logging::init_logging;
pub use tools::build_tool_registry;
