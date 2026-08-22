//! 管理面公开错误与配置加载错误映射

/// 供应商管理错误
///
/// 每个变体面向最终用户（含明确修正建议）。
#[derive(Debug, thiserror::Error)]
pub enum ProviderAdminError {
    /// 入参校验失败（id 非法、必填字段缺失、limit.context 非正整数等）
    #[error("供应商配置校验失败: {0}")]
    Invalid(String),

    /// 创建目标已存在（供应商 id 冲突）
    #[error("供应商配置已存在: {0}")]
    AlreadyExists(String),

    /// 更新 / 删除的目标不存在
    #[error("供应商配置不存在: {0}")]
    NotFound(String),

    /// global 层 fuyao.toml 解析失败（写回前需人工修复）
    #[error("global 层 fuyao.toml 解析失败: {0}")]
    TomlParse(String),

    /// 段结构无法承载写回（providers 段 / 目标条目不是 table、点键写法等）
    #[error("配置段结构非法: {0}")]
    InvalidSection(String),

    /// 文件读写失败
    #[error("配置文件读写失败: {0}")]
    Io(String),
}

/// 把三层配置加载错误映射为公开错误变体
///
/// 列表 API 与 CRUD 共用同一 fail-loud 口径：语法坏 → `TomlParse`（先修复才能
/// 继续管理），值校验失败（providers 段 / 模型必填项）→ `Invalid`，IO → `Io`。
pub fn map_config_error(e: fuyao_api::ConfigError) -> ProviderAdminError {
    match e {
        fuyao_api::ConfigError::TomlError(err) => ProviderAdminError::TomlParse(err.to_string()),
        fuyao_api::ConfigError::InvalidModel(msg) => ProviderAdminError::Invalid(msg),
        fuyao_api::ConfigError::InvalidProvidersSection(msg) => {
            ProviderAdminError::InvalidSection(msg)
        }
        fuyao_api::ConfigError::IoError(err) => ProviderAdminError::Io(err.to_string()),
        fuyao_api::ConfigError::FileNotFound(path) => {
            ProviderAdminError::Io(format!("配置文件不存在: {path}"))
        }
    }
}
