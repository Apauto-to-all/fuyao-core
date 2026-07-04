//! 工具错误类型
//!
//! 定义工具系统中可能出现的错误，包括文件操作、权限、路径安全等场景。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("文件不存在: {0}")]
    FileNotFound(String),

    #[error("路径不是文件: {0}")]
    NotAFile(String),

    #[error("无权限: {0}")]
    PermissionDenied(String),

    #[error("无法读取二进制文件: {0}")]
    BinaryFile(String),

    #[error("读取内容超过安全限制 ({actual} > {max} 字符)")]
    ContentTooLarge { actual: usize, max: usize },

    #[error("路径参数不能为空")]
    EmptyPath,

    #[error("拒绝写入敏感路径: {0}")]
    SensitivePath(String),

    #[error("{0}")]
    IoError(#[from] std::io::Error),
}
