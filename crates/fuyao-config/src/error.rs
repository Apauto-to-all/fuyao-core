//! 配置错误类型

use thiserror::Error;

/// 配置错误类型
#[derive(Debug, Error)]
pub enum ConfigError {
    /// 配置文件读取失败
    #[error("配置文件读取失败: {0}")]
    IoError(#[from] std::io::Error),

    /// TOML 解析失败
    #[error("TOML 解析失败: {0}")]
    TomlError(#[from] toml::de::Error),

    /// 配置文件不存在
    #[error("配置文件不存在: {0}")]
    FileNotFound(String),
}
