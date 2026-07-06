//! Agent 模块
//!
//! 包含 Agent 运行上下文和路径配置。
//! - `context`: Agent 运行上下文（AgentContext、ModelConfig）
//! - `paths`: Agent 三层目录身份证明（AgentPaths）

pub mod context;
pub mod paths;

pub use context::{AgentContext, ModelConfig, SharedAgentCtx};
pub use paths::AgentPaths;
