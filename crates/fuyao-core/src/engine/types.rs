//! 引擎内部共享类型
//!
//! 集中放引擎模块间共享的类型别名、状态类型。

use fuyao_api::message::input::InterruptMessage;
use fuyao_api::{InputEvent, MessageParams};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

/// 会话 ID
///
/// 当前直接用 `String`，与 session crate 的 `Session.id` 类型一致。
/// 采用简单别名而非 newtype，避免与持久化层频繁转换；
/// 后续若类型安全需求增强，可提升为 newtype。
pub type SessionId = String;

/// session 消息队列的载荷
///
/// MessageParams 只对 User 消息有意义（决定本轮用哪个模型），
/// Interrupt/Plugin 不使用（为 None）。
/// 用结构体而非裸 InputEvent，是因为 MessageParams 是额外参数，
/// 和事件一起入队才能在 task 侧拿到。
pub(crate) struct QueuedMessage {
    pub event: InputEvent,
    pub params: Option<MessageParams>,
}

/// 活跃 session 的句柄
///
/// Engine 的调度表（session_id → SessionHandle）持有它。
/// 数据通道与中断通道分离：
/// - `tx_queue`：User / Plugin 事件，task 主循环消费
/// - `tx_interrupt`：Interrupt 事件，task 的 select! 中断点监听
///
/// 分离的原因：select! 监听中断时若和数据共用一个通道，会误取 User/Plugin
/// （它们不是中断信号），处理逻辑变复杂。独立通道保证中断分支只收到 Interrupt。
#[allow(dead_code)]
pub(crate) struct SessionHandle {
    /// 数据通道发送端（User / Plugin）
    pub tx_queue: Sender<QueuedMessage>,
    /// 中断通道发送端（Interrupt）
    pub tx_interrupt: Sender<InterruptMessage>,
    /// session 独立执行流的任务句柄（shutdown 时用于优雅 abort）
    pub task: JoinHandle<()>,
}
