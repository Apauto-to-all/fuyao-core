//! 轮次执行器（TurnExecutor）子模块
//!
//! 队列驱动：从 guide_queue 取消息执行 ReAct 循环，
//! 队列空时阻塞等待 notify 或命令。

use crate::engine::emitter::EventEmitter;
use crate::engine::types::{SharedGuideQueue, SharedPendingQueue, SharedTools};
use fuyao_api::SharedAgentCtx;
use fuyao_provider::Provider as LlmProvider;
use std::sync::Arc;
use tokio::sync::mpsc;

// 子模块声明
mod helpers;
mod outcome;
mod react_loop;

/// 轮次执行器（独立 tokio 任务，常驻运行）
///
/// 队列驱动：从 guide_queue 取消息执行 ReAct 循环，
/// 队列空时阻塞等待 notify 或命令。
pub(crate) struct TurnExecutor {
    /// LLM Provider
    pub(crate) provider: Box<dyn LlmProvider>,
    /// 共享 Agent 运行上下文
    pub(crate) agent_ctx: SharedAgentCtx,
    /// 共享工具注册表
    pub(crate) tools: SharedTools,
    /// 命令通道接收端（Option 便于 run_turn 临时 take 出来用于 select!）
    pub(crate) rx_command: Option<mpsc::Receiver<super::types::TurnCommand>>,
    /// 统一事件发送器
    pub(crate) emitter: EventEmitter,
    /// 引导队列（直接消费）
    pub(crate) guide_queue: SharedGuideQueue,
    /// 排队队列（最终回复完成后转入引导队列）
    pub(crate) pending_queue: SharedPendingQueue,
    /// 队列更新通知器
    pub(crate) queue_notify: Arc<tokio::sync::Notify>,
}
