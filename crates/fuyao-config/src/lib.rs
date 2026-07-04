//! Fuyao 配置模块
//!
//! 支持三层配置合并加载：全局 → Agent 目录 → 工作目录。

pub mod config;
pub mod env;
pub mod error;
pub mod providers;

pub use config::FuyaoConfig;
pub use config::load_config;
pub use config::load_merged_config;
pub use env::load_env;
pub use error::ConfigError;
