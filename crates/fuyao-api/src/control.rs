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
        }
    }
}
