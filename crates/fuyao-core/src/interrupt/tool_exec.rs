//! 工具执行中中断
//!
//! 中断触发时工具正在执行：
//! - 已完成且有结果的工具：正常保留
//! - 正在执行或等待执行的工具：生成中断专用 ToolResult

use crate::engine::EventEmitter;
use crate::interrupt;
use fuyao_api::message::InterruptData;
use fuyao_provider::ToolCallData;

/// 处理工具执行中的中断
///
/// 为所有工具调用发出中断专用 ToolResult（因为 orchestrate 在结果全部收集后才 emit，
/// 中断时还没有任何结果被 emit，所以所有工具调用都需要中断结果）。
/// 中断输出事件已由 InputDispatcher dispatch 管道发出，此处只负责增量结果。
pub(crate) async fn handle(
    emitter: &EventEmitter,
    data: InterruptData,
    tool_calls: &[ToolCallData],
) {
    let source = data.source.clone();
    let reason = data.reason.clone();
    for tc in tool_calls {
        let result = interrupt::make_interrupt_tool_result(
            tc.id.clone(),
            tc.name.clone(),
            source.clone(),
            reason.clone(),
        );
        crate::dispatch::dispatch(
            fuyao_api::message::OutputEvent::ToolResult(result),
            None,
            emitter,
        )
        .await;
    }
}
