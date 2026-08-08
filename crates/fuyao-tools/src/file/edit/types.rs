//! 文件编辑工具类型定义
//!
//! 定义文件编辑工具的结果类型。

/// 替换模式结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct EditReplaceResult {
    /// 是否成功
    pub success: bool,
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
    /// 错误信息
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 补丁模式结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct EditPatchResult {
    /// 是否成功
    pub success: bool,
    /// 修改的文件列表
    pub files_modified: Vec<String>,
    /// 创建的文件列表
    pub files_created: Vec<String>,
    /// 删除的文件列表
    pub files_deleted: Vec<String>,
    /// 差异内容
    pub diff: String,
    /// 警告信息
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// 错误信息
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
