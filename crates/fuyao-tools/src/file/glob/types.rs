//! 文件名搜索工具类型定义
//!
//! 定义文件名搜索工具的参数与结果类型。

/// 默认搜索路径（当前目录）
pub const DEFAULT_PATH: &str = ".";
/// 默认最大结果数
pub const DEFAULT_LIMIT: i64 = 100;

/// glob 工具参数（类型化解析）
#[derive(Debug, serde::Deserialize)]
pub struct GlobArgs {
    /// glob 模式（gitignore 语义：`!` 排除、`{a,b}` 展开、含 `/` 锚定搜索根）
    pub pattern: String,
    /// 搜索路径
    #[serde(default = "default_path")]
    pub path: String,
    /// 最大结果数（钳制到配置硬上限内）
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_path() -> String {
    DEFAULT_PATH.to_string()
}

fn default_limit() -> i64 {
    DEFAULT_LIMIT
}

/// 文件名搜索结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct GlobResult {
    /// 匹配的文件列表
    pub matches: Vec<GlobMatch>,
    /// 总匹配数（全量统计，非截断哨兵值）
    pub total_count: usize,
    /// 是否截断
    pub truncated: bool,
    /// 搜索模式
    pub pattern: String,
    /// 搜索路径
    pub path: String,
    /// 截断提示
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
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
