//! 文件读取工具类型定义
//!
//! 定义文件读取工具的参数与结果类型。参数默认值常量在此声明，
//! schema 构建器（mod.rs）与参数结构体 serde 默认函数共享同一份，
//! 两处不再各写一个数字。

/// 默认起始位置（从 1 开始）
pub const DEFAULT_OFFSET: i64 = 1;
/// 默认读取数量
pub const DEFAULT_LIMIT: i64 = 500;
/// 读取数量上限
pub const MAX_LIMIT: i64 = 2000;

/// read 工具参数（类型化解析）
#[derive(Debug, serde::Deserialize)]
pub struct ReadArgs {
    /// 文件或目录路径（支持绝对路径、相对路径、~/路径）
    pub path: String,
    /// 起始位置（从 1 开始）。文件：行号；目录：条目索引
    #[serde(default = "default_offset")]
    pub offset: i64,
    /// 最大读取数量。文件：行数；目录：条目数
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_offset() -> i64 {
    DEFAULT_OFFSET
}

fn default_limit() -> i64 {
    DEFAULT_LIMIT
}

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
