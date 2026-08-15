//! 工具系统
//!
//! 内置工具注册、执行、安全防护。

mod common;
mod config;
mod file;
mod redact;
mod registry;
mod skill;
mod subagent;
mod terminal;
mod todo;
mod web;

pub use registry::{all_tool_names, all_tools, get_tool};
// 供引擎启动校验挂载（fuyao-app init）调用
pub use terminal::validate_shell_name;
