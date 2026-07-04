//! Todo 工具类型定义
//!
//! 定义 Todo 任务管理工具的结果模型。

/// 任务摘要统计
#[derive(Debug, Clone, serde::Serialize)]
pub struct TodoSummary {
    /// 任务总数
    pub total: usize,
    /// 待处理任务数
    pub pending: usize,
    /// 进行中任务数
    pub in_progress: usize,
    /// 已完成任务数
    pub completed: usize,
    /// 已取消任务数
    pub cancelled: usize,
}

/// Todo 工具结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct TodoWriteResult {
    /// 是否成功
    pub success: bool,
    /// 任务列表
    pub todos: Vec<serde_json::Value>,
    /// 摘要统计
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<TodoSummary>,
    /// 错误信息
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
