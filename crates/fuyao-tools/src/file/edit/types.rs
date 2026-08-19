//! 文件编辑工具类型定义
//!
//! 定义文件编辑工具的参数与结果类型。

/// edit 工具参数（类型化解析）
#[derive(Debug, serde::Deserialize)]
pub struct EditArgs {
    /// 文件路径
    pub path: String,
    /// 要查找的文本
    pub old_string: String,
    /// 替换文本
    pub new_string: String,
    /// 替换所有匹配（默认 false）
    #[serde(default)]
    pub replace_all: bool,
}

/// 查找替换结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct EditReplaceResult {
    /// 文件路径
    pub path: String,
    /// 匹配次数
    pub matches: usize,
    /// 差异内容
    pub diff: String,
    /// 匹配策略
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    /// 警告信息
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}
