//! TurnExecutor 内部使用的中间结果枚举
//!
//! 这些枚举仅在 react_loop 内部使用，对外不暴露。

use fuyao_api::message::InterruptData;
use fuyao_provider::StreamError;

use crate::llm::stream_session;

/// 流式会话完成或中断的结果枚举
pub(super) enum StreamOutcome {
    /// 流式会话正常完成
    Completed(Result<stream_session::StreamResult, StreamError>),
    /// 收到中断命令
    Interrupted(InterruptData),
}

/// 工具执行完成或中断的结果枚举
pub(super) enum ToolExecOutcome {
    /// 工具执行正常完成
    Completed,
    /// 收到中断命令
    Interrupted(InterruptData),
}

/// 中断时的流式阶段（从 MutexGuard 中克隆出来的 owned 值）
pub(super) enum Phase {
    Streaming,
    Backoff,
}
