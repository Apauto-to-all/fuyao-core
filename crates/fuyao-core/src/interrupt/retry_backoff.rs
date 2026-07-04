//! LLM 重试/退避期间中断
//!
//! 中断触发时引擎正在重试退避等待中，直接终止轮次。
//! 中断输出事件已由 InputDispatcher dispatch 管道发出，此处无需额外处理。

/// 处理 LLM 重试/退避期间的中断
///
/// 无增量内容需要保存，无状态清理，空操作。
pub(crate) async fn handle() {
    // 中断输出事件已由 InputDispatcher dispatch 管道发出
}
