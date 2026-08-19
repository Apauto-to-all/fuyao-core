//! Todo 工具类型定义
//!
//! 定义 Todo 任务管理工具的参数与结果模型。

/// todo 项的 wire 输入形态（status 容错收敛在 handler：非法值回退 pending）
#[derive(Debug, serde::Deserialize)]
pub struct TodoItemInput {
    /// 唯一标识
    pub id: String,
    /// 任务描述
    pub content: String,
    /// 状态（pending / in_progress / completed / cancelled，缺省 pending）
    #[serde(default)]
    pub status: String,
}

/// todowrite 工具参数（类型化解析）
///
/// `todos` 缺省 = 读取当前列表；给出 = 整体覆盖写入。
#[derive(Debug, serde::Deserialize)]
pub struct TodoWriteArgs {
    /// 任务数组。不传则读取。
    pub todos: Option<Vec<TodoItemInput>>,
}

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
    /// 任务列表
    pub todos: Vec<serde_json::Value>,
    /// 摘要统计
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<TodoSummary>,
}
