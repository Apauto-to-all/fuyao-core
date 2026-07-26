//! 工具模块
//!
//! 定义工具系统相关的类型，遵循 OpenAI Function Calling 规范。
//! - `definition`: 工具定义（ToolDefinition、ToolSchema、ToolParameters、ToolParameterProperty）
//! - `result`: 工具执行结果（ToolResult）
//! - `func`: 工具执行器函数类型（ToolFn）
//! - `context`: 工具调用上下文（ToolCallContext）

pub mod context;
pub mod definition;
pub mod func;
pub mod result;

pub use context::ToolCallContext;
pub use definition::{ToolDefinition, ToolParameterProperty, ToolParameters, ToolSchema};
pub use func::ToolFn;
pub use result::ToolResult;

// 取消令牌：工具 handler 据此响应中断 / shutdown，优雅收尾长任务
pub use tokio_util::sync::CancellationToken;
