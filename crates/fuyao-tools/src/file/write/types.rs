//! 文件写入工具类型定义
//!
//! 定义文件写入工具的参数与结果类型。

/// write 工具参数（类型化解析）
#[derive(Debug, serde::Deserialize)]
pub struct WriteArgs {
    /// 文件路径（不存在则创建，存在则覆盖）
    pub path: String,
    /// 要写入的完整内容
    pub content: String,
}

/// 文件写入结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct WriteResult {
    /// 文件路径
    pub path: String,
    /// 写入字节数（无变化短路时为 0）
    pub bytes_written: usize,
    /// 是否新建文件
    pub created: bool,
    /// 覆写差异：常规改动为完整 unified diff，全量改写为旧内容账本
    ///（新增侧折叠；删除侧是旧内容覆写后唯一留存的副本，可据此恢复）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// 新内容与原文件一致（判定基准与 diff 展示一致：剥离 BOM、行尾归一化），
    /// 已跳过落盘，磁盘字节保持原样（含原有 BOM / CRLF）
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unchanged: bool,
}
