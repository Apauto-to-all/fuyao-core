//! 工具执行器函数类型
//!
//! 定义工具执行器的函数签名，用于注册和调用工具。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

/// 工具执行器函数类型
///
/// 接收 LLM 传入的 JSON 参数、框架注入的调用上下文、取消令牌，返回工具执行结果字符串。
///
/// # 参数
/// - `serde_json::Value`: LLM 传入的工具调用参数
/// - `crate::ToolCallContext`: 框架注入的调用上下文（会话 ID、Agent 路径等）
/// - `CancellationToken`: 本次工具批次的中断信号（派生自 session shutdown_token）
///
///   短任务 handler 可忽略（参数加占位下划线，被 abort 即可，双保险兜底）；
///   长任务 handler 应在内部 `select!` 监听 `cancelled()`，命中后优雅收尾
///   （杀子进程 / 释放外部资源）并返回取消标记。
///
/// # 返回
/// - `String`: 工具执行结果的字符串表示
pub type ToolFn = Arc<
    dyn Fn(
            serde_json::Value,
            crate::ToolCallContext,
            CancellationToken,
        ) -> Pin<Box<dyn Future<Output = String> + Send>>
        + Send
        + Sync,
>;
