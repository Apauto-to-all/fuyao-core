//! 控制通道载荷
//!
//! 控制通道承载「命令主循环做事」的信号——非用户内容（不入 ReAct 队列）、
//! 非抢占（不打断在途 turn，等当前 turn 结束后在边界处理）、非独立转发通知。
//! 每个变体代表一种指令；新增控制类功能 = 加变体，不开新通道。
//!
//! 注意：本类型是控制信号，不属于消息（非 InputEvent / OutputEvent），
//! 仅在引擎内部控制通道流转。

/// 控制通道载荷：命令主循环在 turn 边界执行的信号
///
/// 控制通道承载「命令主循环做事」的信号——非用户内容（不入 ReAct 队列）、
/// 非抢占（不打断在途 turn，等当前 turn 结束后在边界处理）、非独立转发通知。
/// 每个变体代表一种指令；新增控制类功能 = 加变体，不开新通道。
pub enum ControlCommand {
    /// 手动触发上下文压缩（跳过阈值 / 反抖动，触发原因标记为 manual）
    Compress,
    /// 对话回退：删目标 seq 之后的所有消息并重算会话状态
    ///
    /// 携带 `target_seq`（回退到的目标消息 seq）。目标必须是用户消息或压缩消息，
    /// 合法性由 store 层回退执行体（`SessionStore::rollback_to`）在单事务内原子校验。
    ///
    /// 与 `Compress` 在控制通道里地位对等：都是 DB 写命令，都靠 task 在 turn 边界
    /// 串行执行保证安全，都不抢占在途 turn。回退结果经 per-session 出口以
    /// `OutputEvent::Rollback` 事件流出，**不走请求-响应通道**——控制通道是
    /// fire-and-forget 的命令载体，回执由事件出口承担（同压缩）。
    Rollback { target_seq: i64 },
}

/// 控制命令对 turn 的处置指令
///
/// 每条 [`ControlCommand`] 自带一重语义——它指示执行后 turn 该怎么处置。
/// 这是命令的固有属性（命令定义时即确定），不是执行后的结果：
/// - `Continue`：执行后继续当前 turn（不改变 turn 持有的状态）
/// - `StopTurn`：执行后 turn 退出（改变了 turn 持有的状态，继续跑无意义 / 会不一致）
///
/// 间隙检查点（run_turn 内 ReAct loop 顶部）取到命令时调
/// [`ControlCommand::turn_directive`] 即知该不该停，无需执行后再判断。
pub enum TurnDirective {
    /// 命令执行后，turn 继续（不改变 turn 持有的状态）
    Continue,
    /// 命令执行后，turn 退出（改变了 turn 持有的状态，继续跑无意义 / 会不一致）
    StopTurn,
}

impl ControlCommand {
    /// 这条控制命令对 turn 的处置指令
    ///
    /// 命令的固有属性——执行前即可知。间隙检查点据此决定取到命令后要不要 return。
    pub fn turn_directive(&self) -> TurnDirective {
        match self {
            // 手动压缩：压缩后原始消息被摘要替代，turn 持有的消息列表 / 边界失效 → StopTurn
            ControlCommand::Compress => TurnDirective::StopTurn,
            // 回退：删了消息 + 重算 count，turn 持有的 session 状态失效 → StopTurn
            ControlCommand::Rollback { .. } => TurnDirective::StopTurn,
        }
    }
}
