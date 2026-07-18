//! 引擎内部共享类型
//!
//! 集中放引擎模块间共享的类型别名、状态类型。

use fuyao_api::MessageParams;
use fuyao_api::message::input::InterruptMessage;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

// InboundUser 已提升到 fuyao-api，供 fuyao-hooks 的 SessionSender 引用（避免 hooks 反向依赖 core）
pub(crate) use fuyao_api::InboundUser;

/// 会话 ID
///
/// 当前直接用 `String`，与 session crate 的 `Session.id` 类型一致。
/// 采用简单别名而非 newtype，避免与持久化层频繁转换；
/// 后续若类型安全需求增强，可提升为 newtype。
pub type SessionId = String;

/// 队列消息（内容 + 消息参数，mode 已在入队时分流）
///
/// guide / pending 两个队列装同一种消息。mode 在入站消息过管道的 process 段
/// 入队时已按 Guide/Pending 分流到对应对列，队列内不再区分。
pub(crate) struct QueuedUserMessage {
    /// 消息文本
    pub content: String,
    /// 消息参数（model id 等，跟着每条消息走）
    pub params: MessageParams,
}

/// 共享队列（guide / pending 对等，同类型，可互倒）
pub(crate) type SharedQueue = Arc<Mutex<VecDeque<QueuedUserMessage>>>;

/// 活跃 session 的句柄
///
/// Engine 的调度表（session_id → SessionHandle）持有它。
/// 双队列 + 入站通道 + 中断通道分离：
/// - `guide`：引导队列，直接消费，驱动 ReAct 循环
/// - `pending`：排队队列，AI 不再调工具（最终回复）后才解禁转入 guide
/// - `tx_inbound`：入站通道（User 消息送进 session task 过管道）
/// - `tx_interrupt`：中断通道，select! 中断点监听（与队列正交）
#[allow(dead_code)]
pub(crate) struct SessionHandle {
    /// 引导队列（直接消费）
    pub guide: SharedQueue,
    /// 排队队列（最终回复后转入 guide）
    pub pending: SharedQueue,
    /// 入站通道发送端（User 消息）
    pub tx_inbound: Sender<InboundUser>,
    /// 中断通道发送端（Interrupt）
    pub tx_interrupt: Sender<InterruptMessage>,
    /// session 独立执行流的任务句柄（shutdown 时用于优雅 abort）
    pub task: JoinHandle<()>,
}
