//! 终端工具类型定义
//!
//! 定义 bash 工具返回给 AI 的结果结构体。

/// bash 工具返回结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct BashToolResult {
    /// 是否成功（exit_code == 0）
    pub success: bool,
    /// 标准输出（stdout + stderr 合并）
    pub output: String,
    /// 进程退出码
    pub exit_code: i32,
    /// 执行错误信息
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 是否超时
    #[serde(skip_serializing_if = "is_false")]
    pub timed_out: bool,
    /// 是否被取消（中断 / shutdown 触发，与超时正交）
    #[serde(skip_serializing_if = "is_false")]
    pub cancelled: bool,
    /// 命令执行时的工作目录
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// 执行耗时（毫秒）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_time_ms: Option<f64>,
    /// 退出码解读
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code_meaning: Option<String>,
    /// Shell 类型
    pub shell_type: String,
}

/// 用于 serde skip_serializing_if 的辅助函数
fn is_false(b: &bool) -> bool {
    !*b
}
