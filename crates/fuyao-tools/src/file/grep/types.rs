//! 文件内容搜索工具类型定义
//!
//! 定义文件内容搜索工具的参数与结果类型。

/// 默认搜索路径（当前目录）
pub const DEFAULT_PATH: &str = ".";
/// 默认最大结果数
pub const DEFAULT_LIMIT: i64 = 50;
/// 最大结果数上限
pub const MAX_LIMIT: i64 = 100;

/// grep 工具参数（类型化解析）
#[derive(Debug, serde::Deserialize)]
pub struct GrepArgs {
    /// 正则表达式
    pub pattern: String,
    /// 搜索路径
    #[serde(default = "default_path")]
    pub path: String,
    /// 文件过滤模式（如 *.py、*.{ts,tsx}）
    pub include: Option<String>,
    /// 最大结果数
    #[serde(default = "default_limit")]
    pub limit: i64,
    /// 跳过前 N 个结果（分页用）
    #[serde(default)]
    pub offset: i64,
    /// 显示匹配行的上下文行数
    #[serde(default)]
    pub context: i64,
}

fn default_path() -> String {
    DEFAULT_PATH.to_string()
}

fn default_limit() -> i64 {
    DEFAULT_LIMIT
}

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
