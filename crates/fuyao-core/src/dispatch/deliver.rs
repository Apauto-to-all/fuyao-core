//! 发送 + 观察步骤
//!
//! 将事件推送到 CLI 渲染通道，并通知观察钩子。

use crate::engine::EventEmitter;
use fuyao_api::message::OutputEvent;

/// 发送事件到 CLI + 触发观察钩子
///
/// 暴露为 `pub(crate)` 供 TurnExecutor 在消费队列时直接调用（延迟 deliver 场景）。
pub(crate) async fn run(emitter: &EventEmitter, event: OutputEvent) {
    let hooks = emitter.hooks().lock().await;
    let _ = emitter.send(event.clone()).await;
    hooks.hook_output_observe(event).await;
}
