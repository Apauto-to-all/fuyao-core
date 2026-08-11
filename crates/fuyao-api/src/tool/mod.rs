//! 工具模块
//!
//! 定义工具系统相关的类型，遵循 OpenAI Function Calling 规范。
//! - `definition`: 工具定义（ToolDefinition、ToolSchema、ToolParameters、ToolParameterProperty）
//! - `entry`: 工具条目（ToolEntry = schema + handler + 可见性元数据）
//! - `result`: 工具执行结果（ToolResult）
//! - `func`: 工具执行器函数类型（ToolFn）
//! - `context`: 工具调用上下文（ToolCallContext）
//! - `ops`: 运行期能力注入接口（SubagentOps + ChildSessionSource + TodoStoreOps）

pub mod context;
pub mod definition;
pub mod entry;
pub mod func;
pub mod ops;
pub mod result;

pub use context::{ToolCallContext, ToolCapabilities};
pub use definition::{
    ToolDefinition, ToolDefinitionBuilder, ToolParameterProperty, ToolParameters, ToolSchema,
};
pub use entry::ToolEntry;
pub use func::ToolFn;
pub use ops::{ChildSessionSource, SubagentOps, TodoStoreOps};
pub use result::ToolResult;

// 取消令牌：工具 handler 据此响应中断 / shutdown，优雅收尾长任务
pub use tokio_util::sync::CancellationToken;
