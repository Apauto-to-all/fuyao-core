//! 文件内容搜索工具类型定义
//!
//! 定义文件内容搜索工具的参数与结果类型。

/// 默认搜索路径（当前目录）
pub const DEFAULT_PATH: &str = ".";
/// 默认最大结果数
pub const DEFAULT_LIMIT: i64 = 50;

/// grep 工具参数（类型化解析）
#[derive(Debug, serde::Deserialize)]
pub struct GrepArgs {
    /// 正则表达式
    pub pattern: String,
    /// 搜索路径
    #[serde(default = "default_path")]
    pub path: String,
    /// glob 过滤模式（如 *.rs、*.{ts,tsx}，`!` 前缀排除；无斜杠模式跨目录匹配）
    pub glob: Option<String>,
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

/// 文件内容搜索结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct GrepResult {
    /// 匹配的行列表
    pub matches: Vec<GrepMatch>,
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

/// 内容匹配项
#[derive(Debug, Clone, serde::Serialize)]
pub struct GrepMatch {
    /// 文件路径
    pub file: String,
    /// 行号
    pub line: u64,
    /// 匹配内容（超长行按字符截断，尾部以省略标记 `…` 示意）
    pub content: String,
}
