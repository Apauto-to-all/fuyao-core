//! 文件内容搜索工具类型定义
//!
//! 定义文件内容搜索工具的结果类型。

/// 文件内容搜索结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct GrepResult {
    /// 匹配的行列表
    pub matches: Vec<GrepMatch>,
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

/// 内容匹配项
#[derive(Debug, Clone, serde::Serialize)]
pub struct GrepMatch {
    /// 文件路径
    pub file: String,
    /// 行号
    pub line: u64,
    /// 匹配内容
    pub content: String,
    /// 上下文
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}
