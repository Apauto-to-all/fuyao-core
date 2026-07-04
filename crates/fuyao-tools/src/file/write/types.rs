//! 文件写入工具类型定义
//!
//! 定义文件写入工具的结果类型。

/// 文件写入结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct WriteResult {
    /// 是否成功
    pub success: bool,
    /// 结果描述（"文件已创建" 或 "文件已覆盖"）
    pub result: String,
    /// 文件路径
    pub path: String,
    /// 写入字节数
    pub bytes_written: usize,
    /// 是否新建文件
    pub created: bool,
    /// 警告信息（外部编辑检测）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}
