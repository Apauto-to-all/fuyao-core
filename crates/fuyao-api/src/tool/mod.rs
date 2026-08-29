//! 工具模块
//!
//! 定义工具系统相关的类型（中立形态，wire 编码归供应商适配层）。
//! - `call`: 工具调用数据（ToolCallData，一次工具调用的中立表示）
//! - `definition`: 工具定义（ToolDefinition、ToolParameters、ToolParameterProperty）
//! - `entry`: 工具条目（ToolEntry = schema + handler + 可见性元数据 + insert_tool 注册）
//! - `output`: 工具执行结果信封（ToolOutput / ToolError，"error" 键约定类型化）
//! - `func`: 工具执行器函数类型（ToolFn）与 handler 侧辅助（tool_handler / parse_args）
//! - `context`: 工具调用上下文（ToolCallContext）
//! - `ops`: 运行期能力注入接口（SubagentOps + ChildSessionSource + TodoStoreOps）

pub mod call;
pub mod context;
pub mod definition;
pub mod entry;
pub mod func;
pub mod ops;
pub mod output;

pub use call::ToolCallData;
pub use context::{ToolCallContext, ToolCapabilities};
pub use definition::{
    ToolDefinition, ToolDefinitionBuilder, ToolParameterProperty, ToolParameters,
};
pub use entry::{ToolEntry, insert_tool};
pub use func::{ToolFn, parse_args, tool_handler};
pub use ops::{ChildSessionSource, SubagentError, SubagentOps, TodoStoreOps};
pub use output::{ToolError, ToolOutput};

// 取消令牌：工具 handler 据此响应中断 / shutdown，优雅收尾长任务
pub use tokio_util::sync::CancellationToken;
