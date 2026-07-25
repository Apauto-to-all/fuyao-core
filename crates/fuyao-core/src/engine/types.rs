//! 引擎内部共享类型
//!
//! 集中放引擎模块间共享的类型别名、状态类型。

use fuyao_api::SessionParams;
use fuyao_api::message::input::{InterruptMessage, PluginMessage};
use fuyao_api::message::output::UserMessage as OutputUserMessage;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// 会话 ID
///
/// 当前直接用 `String`，与 session crate 的 `Session.id` 类型一致。
/// 采用简单别名而非 newtype，避免与持久化层频繁转换；
/// 后续若类型安全需求增强，可提升为 newtype。
pub type SessionId = String;

/// 共享队列（guide / pending 对等，同类型，可互倒）
///
/// 队列载荷直接复用 output 侧 `OutputUserMessage`——内核统一处理输出侧消息
///（见 AGENTS.md「消息处理原则」），入站载荷与队列载荷类型完全一致，
/// 复用同一类型避免无意义的拆解/重组（也消除字段丢失风险）。
pub(crate) type SharedQueue = Arc<StdMutex<VecDeque<OutputUserMessage>>>;

/// 活跃 session 的句柄
///
/// Engine 的调度表（session_id → SessionHandle）持有它。
/// 双队列 + 入站通道 + 中断通道 + Plugin 通道 + SessionParams 共享句柄 + 关闭信号分离：
/// - `guide`：引导队列，直接消费，驱动 ReAct 循环
/// - `pending`：排队队列，AI 不再调工具（最终回复）后才解禁转入 guide
/// - `tx_inbound`：入站通道（User 消息送进 session task 过管道）
/// - `tx_interrupt`：中断通道，select! 中断点监听（与队列正交）
/// - `tx_plugin`：Plugin 通道，插件通知送进 session task 过 dispatch 管道（不参与 ReAct）
/// - `session_params`：对话级参数共享句柄，Engine 写（update_session_params）、task 现读现用
/// - `task`：session 独立执行流任务句柄（shutdown 时 await 等退出 / 超时 abort 兜底）
/// - `shutdown_token`：该 session 的关闭信号（Engine::shutdown 时 cancel）
#[allow(dead_code)]
pub(crate) struct SessionHandle {
    /// 引导队列（直接消费）
    pub guide: SharedQueue,
    /// 排队队列（最终回复后转入 guide）
    pub pending: SharedQueue,
    /// 入站通道发送端（User 消息，output 侧 OutputUserMessage）
    pub tx_inbound: Sender<OutputUserMessage>,
    /// 中断通道发送端（Interrupt）
    pub tx_interrupt: Sender<InterruptMessage>,
    /// Plugin 通道发送端（Plugin 通知，过 dispatch 管道发外部）
    pub tx_plugin: Sender<PluginMessage>,
    /// 对话级参数共享句柄（与 SessionCtx.session_params 指向同一份）
    ///
    /// Engine 的 update_session_params 经此写回；task 内的消费点（跑 turn、压缩）
    /// 现读 SessionCtx.session_params。整 session 全程只有一份，无"生效时机"概念。
    pub session_params: Arc<Mutex<SessionParams>>,
    /// session 独立执行流的任务句柄（shutdown 时 await 等退出 / 超时 abort 兜底）
    pub task: JoinHandle<()>,
    /// 该 session 的关闭信号（Engine::shutdown 时 cancel，task select! 监听）
    ///
    /// 派生自引擎级 shutdown_token（`child_token()`），目前 Engine::shutdown
    /// 一次性 cancel 所有 session；保留 child_token 形态为未来「单 session 销毁」扩展点。
    pub shutdown_token: CancellationToken,
}
