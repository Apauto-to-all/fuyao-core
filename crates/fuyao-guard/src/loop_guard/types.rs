//! 循环检测类型定义

/// 循环严重程度
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopSeverity {
    /// 警告：追加提示信息
    Warn,
    /// 注入：替换工具结果内容
    Inject,
    /// 中断：发送中断信号
    Interrupt,
    /// 终止：彻底停止
    Abort,
}

/// 循环类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopType {
    /// 工具调用循环
    Tool,
    /// 文本内容重复
    Text,
}

/// 工具调用历史记录
#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    /// 工具名称
    pub tool_name: String,
    /// 规范化后的参数字符串，用于循环比对
    pub canonical_args: String,
}
