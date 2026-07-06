//! 引擎共享类型定义
//!
//! Engine（InputDispatcher）与 TurnExecutor 之间的"协议层"：
//! - 共享队列类型别名
//! - 入队消息结构（QueuedUserMessage）
//! - 命令枚举（Engine → TurnExecutor 单向通道）

use fuyao_api::message::input;
use fuyao_api::message::output;
use std::collections::VecDeque;
use std::sync::Arc;

/// 入队用户消息
///
/// 同时携带原始输入数据与拦截后的输出事件体：
/// - `user_data`：用于按 mode 分流入队（Guide/Pending）
/// - `message`：拦截器可能修改过的输出 UserMessage，消费时直接 deliver
///
/// 拦截与 deliver 解耦，保证 session_mgr 观察顺序与 AI 回复严格交替。
pub(crate) struct QueuedUserMessage {
    /// 原始输入数据（保留 payload.mode/source 等元信息）
    pub(crate) user_data: input::UserMessage,
    /// 拦截后的输出事件体（消费时包装为 OutputEvent::User 后 deliver）
    pub(crate) message: output::UserMessage,
}

/// 共享工具注册表
pub type SharedTools = Arc<
    std::sync::Mutex<(
        std::collections::HashMap<String, fuyao_api::ToolFn>,
        Vec<serde_json::Value>,
    )>,
>;

// SharedHooks 已下沉到 fuyao-hooks（Plugin trait 签名需要，避免环依赖）
pub use fuyao_hooks::SharedHooks;

/// 共享引导队列
pub(crate) type SharedGuideQueue = Arc<std::sync::Mutex<VecDeque<QueuedUserMessage>>>;

/// 共享排队队列
pub(crate) type SharedPendingQueue = Arc<std::sync::Mutex<VecDeque<QueuedUserMessage>>>;

/// TurnExecutor 命令
pub(crate) enum TurnCommand {
    /// 中断当前轮次
    Interrupt(input::InterruptMessage),
    /// 停止执行器
    Stop,
}
