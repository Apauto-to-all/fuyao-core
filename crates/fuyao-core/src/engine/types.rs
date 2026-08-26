//! 引擎内部共享类型
//!
//! 集中放引擎模块间共享的类型别名、状态类型。

use fuyao_api::SessionParams;
use fuyao_api::message::QueueEntry;
use fuyao_api::message::output::InterruptMessage as OutputInterruptMessage;
use fuyao_hooks::NamedPluginInstance;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;
use tokio::sync::mpsc::Sender;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// 会话 ID
///
/// 当前直接用 `String`，与 session crate 的 `Session.id` 类型一致。
/// 采用简单别名而非 newtype，避免与持久化层频繁转换；
/// 后续若类型安全需求增强，可提升为 newtype。
pub type SessionId = String;

/// 共享队列（guide / pending 对等，同类型，可互倒）
pub(crate) type SharedQueue = Arc<StdMutex<VecDeque<QueueEntry>>>;

/// 会话执行流的 turn 相位（Engine 与 session task 共享的运行状态）
///
/// task 侧在进入「会写库的 turn 区间」（pre-turn 压缩 → 注入 → run_turn）前置
/// `Running`、区间结束后回 `Idle`；Engine 侧的 [`crate::engine::stop_session`]
/// 据 watch 值判断是否有 turn 在跑，并等回 `Idle` 实现屏障语义——
/// session task 是该 session DB 写入的唯一执行者，相位回 `Idle` 即代表
/// 本 session 已无任何在途写库（中断收尾的补发落库在 turn 返回前已完成）。
///
/// watch 通道承载：task 持 sender（经 `SessionCtx.turn_phase`）更新，
/// Engine 持 receiver（经 `SessionHandle.turn_phase_rx`）等待。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnPhase {
    /// 无 turn 在跑：task 停在主循环边界 / idle select!
    Idle,
    /// turn 区间运行中：pre-turn 压缩 → 注入 → run_turn 全程（含中断收尾落库）
    Running,
}

/// turn 区间的相位守卫：构造置 `Running`，drop 回 `Idle`
///
/// Drop 语义保证提前 return / panic 时相位也能回 `Idle`——相位回落的时点
/// 晚于 turn 的全部落库（含中断收尾补发），回落即代表 DB 已静默。
/// watch sender 发送失败（Engine 侧 receiver 已全部 drop，session 已移出调度表）
/// 时忽略——无等待方，相位无观察者。
pub(crate) struct TurnPhaseGuard {
    tx: watch::Sender<TurnPhase>,
}

impl TurnPhaseGuard {
    /// 进入 turn 区间：置 `Running`
    pub(crate) fn enter(tx: &watch::Sender<TurnPhase>) -> Self {
        // 接收端全 drop 时 send 失败：无等待方，忽略即可
        let _ = tx.send(TurnPhase::Running);
        Self { tx: tx.clone() }
    }
}

impl Drop for TurnPhaseGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(TurnPhase::Idle);
    }
}

/// 活跃 session 的句柄
///
/// Engine 的调度表（session_id → SessionHandle）持有它。
/// 双队列 + 入站通道 + 中断通道 + SessionParams 共享句柄 + 关闭信号 + 插件实例分离：
/// - `guide`：引导队列，直接消费，驱动 ReAct 循环
/// - `pending`：排队队列，AI 不再调工具（最终回复）后才解禁转入 guide
/// - `tx_inbound`：统一入站通道（外部用户消息 / 控制命令与插件注入的 User 条目
///   共同经此送进 session task，保证总序）
/// - `tx_interrupt`：中断通道，select! 中断点监听（与队列正交）
/// - `turn_phase_rx`：turn 相位接收端（stop_session 屏障等待用）
/// - `session_params`：对话级参数共享句柄，Engine 写 / task 现读现用
/// - `task`：session 独立执行流任务句柄（shutdown 时 await 等退出 / 超时 abort 兜底）
/// - `shutdown_token`：该 session 的关闭信号（Engine::shutdown 时 cancel）
/// - `plugin_instances`：该 session 的插件实例集合（session 结束时逆序 dispose）
#[allow(dead_code)]
pub(crate) struct SessionHandle {
    /// 引导队列（直接消费）
    pub guide: SharedQueue,
    /// 排队队列（最终回复后转入 guide）
    pub pending: SharedQueue,
    /// 统一入站通道发送端（外部入站与插件注入的条目统一承载，保证总序）
    pub tx_inbound: Sender<QueueEntry>,
    /// 中断通道发送端（output 侧 InterruptMessage，入口转化后承载）
    pub tx_interrupt: Sender<OutputInterruptMessage>,
    /// turn 相位接收端（与 SessionCtx.turn_phase 的 sender 同源）
    ///
    /// task 侧进 turn 区间置 Running / 退出回 Idle；stop_session 借此实现
    /// 「等 turn 完全终止（含收尾落库）」的屏障语义。
    pub turn_phase_rx: watch::Receiver<TurnPhase>,
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
    /// 该 session 的插件实例集合（装配期由 assemble_session_hooks 生成）
    ///
    /// 生命周期闭环：session 结束（end_session / shutdown）时由收尾 helper
    /// 逆序逐个 dispose（后注册的先销毁），插件资源不随 session 泄漏。
    /// hooks 已在 SessionCtx 内冻结共享，本字段只承载 dispose 义务。
    pub plugin_instances: Vec<NamedPluginInstance>,
}
