//! 工具执行器函数类型
//!
//! 定义工具执行器的函数签名，用于注册和调用工具。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// 工具执行器函数类型
///
/// 接收 LLM 传入的 JSON 参数和框架注入的调用上下文，返回工具执行结果字符串。
///
/// # 参数
/// - `serde_json::Value`: LLM 传入的工具调用参数
/// - `crate::ToolCallContext`: 框架注入的调用上下文（会话 ID、Agent 路径等）
///
/// # 返回
/// - `String`: 工具执行结果的字符串表示
pub type ToolFn = Arc<
    dyn Fn(
            serde_json::Value,
            crate::ToolCallContext,
        ) -> Pin<Box<dyn Future<Output = String> + Send>>
        + Send
        + Sync,
>;
