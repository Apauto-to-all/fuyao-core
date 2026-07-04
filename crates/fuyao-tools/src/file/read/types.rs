//! 文件读取工具类型定义
//!
//! 定义文件读取工具的结果类型。

/// 文件读取结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReadResult {
    /// 文件内容（带行号）
    pub result: String,
    /// 文件路径
    pub path: String,
    /// 总行数
    pub total_lines: usize,
    /// 文件大小（字节）
    pub file_size: u64,
    /// 起始位置
    pub offset: usize,
    /// 读取数量
    pub limit: usize,
    /// 是否截断
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    /// 截断提示
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// 大文件提示
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _hint: Option<String>,
}

/// 目录列表结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct DirectoryResult {
    /// 目录条目列表
    pub result: Vec<DirectoryEntry>,
    /// 目录路径
    pub path: String,
    /// 总条目数
    pub total_count: usize,
    /// 是否截断
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    /// 截断提示
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// 目录条目
#[derive(Debug, Clone, serde::Serialize)]
pub struct DirectoryEntry {
    /// 文件/目录名
    pub name: String,
    /// 类型（file 或 dir）
    #[serde(rename = "type")]
    pub entry_type: String,
    /// 文件大小（字节，仅文件）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}
