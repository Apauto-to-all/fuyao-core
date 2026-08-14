//! 配置加载错误类型

use thiserror::Error;

/// 配置错误类型
///
/// 涵盖三层配置合并加载过程中的所有失败场景：文件读取、TOML 解析。
/// 反序列化合并表时产生的 `toml::de::Error` 与 TOML 解析错误同源，复用 `TomlError`。
#[derive(Debug, Error)]
pub enum ConfigError {
    /// 配置文件读取失败
    #[error("配置文件读取失败: {0}")]
    IoError(#[from] std::io::Error),

    /// TOML 解析或反序列化失败
    #[error("TOML 解析失败: {0}")]
    TomlError(#[from] toml::de::Error),

    /// 配置文件不存在
    #[error("配置文件不存在: {0}")]
    FileNotFound(String),

    /// 配置内容校验失败（如 Provider / Model 条目残缺：非 table、缺 `name`、
    /// `name` 非字符串；模型条目缺 `limit.context` 或值非法）
    #[error("模型配置无效: {0}")]
    InvalidModel(String),

    /// `providers` 段结构错误（顶层键存在但值不是 table，
    /// 如误写 `providers = "deepseek"`、`providers = 123`、`providers = ["a"]`）
    #[error("供应商配置段无效: {0}")]
    InvalidProvidersSection(String),
}
