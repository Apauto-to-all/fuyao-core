//! Agent 模块
//!
//! 包含引擎交互参数三件套和路径配置。
//! - `params`: 引擎交互参数三件套（EngineParams / SessionParams / MessageParams
//!   + 内层 ModelConfig / AgentConfig）
//! - `paths`: Agent 三层目录身份证明（AgentPaths）

pub mod params;
pub mod paths;

pub use params::{AgentConfig, EngineParams, MessageParams, ModelConfig, SessionParams};
pub use paths::AgentPaths;
