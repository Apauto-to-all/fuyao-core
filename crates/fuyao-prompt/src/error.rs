//! Prompt 错误类型

use thiserror::Error;

/// Prompt 模块错误
#[derive(Debug, Error)]
pub enum PromptError {
    /// 文件读取失败
    #[error("文件读取失败: {0}")]
    Io(#[from] std::io::Error),

    /// YAML 解析失败
    #[error("YAML 解析失败: {0}")]
    Yaml(#[from] serde_yaml::Error),
}
