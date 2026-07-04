//! 工具系统
//!
//! 内置工具注册、执行、安全防护。

pub mod common;
pub mod config;
pub mod error;
pub mod file;
pub mod redact;
pub mod registry;
pub mod skill;
pub mod terminal;
pub mod todo;
pub mod web;

pub use error::ToolError;
pub use registry::{ToolEntry, all_tool_names, all_tools, get_tool};
