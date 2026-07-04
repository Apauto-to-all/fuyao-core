//! 拦截步骤
//!
//! 调用 hook_output_intercept，插件可修改或阻断事件。

use crate::engine::EventEmitter;
use fuyao_api::message::OutputEvent;
use fuyao_hooks::InterceptResult;

/// 执行拦截钩子
///
/// 返回 Pass(可能被修改的事件) 或 Block。
pub async fn run(emitter: &EventEmitter, event: OutputEvent) -> InterceptResult<OutputEvent> {
    let hooks = emitter.hooks().lock().await;
    hooks.hook_output_intercept(&event)
}
