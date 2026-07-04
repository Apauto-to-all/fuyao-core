//! 文件名搜索工具类型定义
//!
//! 定义文件名搜索工具的结果类型。

/// 文件名搜索结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct GlobResult {
    /// 匹配的文件列表
    pub matches: Vec<GlobMatch>,
    /// 总匹配数
    pub total_count: usize,
    /// 是否截断
    pub truncated: bool,
    /// 搜索模式
    pub pattern: String,
    /// 搜索路径
    pub path: String,
    /// 错误信息
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 截断提示
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _hint: Option<String>,
}

/// 文件名匹配项
#[derive(Debug, Clone, serde::Serialize)]
pub struct GlobMatch {
    /// 文件路径
    pub path: String,
    /// 文件大小（字节）
    pub size: u64,
    /// 修改时间戳（秒）
    pub modified: u64,
}
