//! ReAct 循环间隙 / 无活跃轮次中断
//!
//! 中断触发时无增量内容需要保存。
//! 中断输出事件已由 InputDispatcher dispatch 管道发出，此处无需额外处理。

/// 处理 ReAct 循环间隙或无活跃轮次时的中断
///
/// 无增量内容，无状态清理，空操作。
pub(crate) async fn handle() {
    // 中断输出事件已由 InputDispatcher dispatch 管道发出
}
