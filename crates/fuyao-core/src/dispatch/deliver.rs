//! 发送 + 观察步骤
//!
//! 将事件经 `Emitter::emit` 推到出口通道（全引擎唯一发送出口），
//! 再触发观察钩子。发送在前、观察在后（串行，顺序确定）。

use crate::emit::Emitter;
use fuyao_api::message::OutputEvent;
use fuyao_hooks::SharedHooks;

/// 发送事件到出口通道 + 触发观察钩子
///
/// 顺序：先 `Emitter::emit`（盖 session_id + tx.send），后 `hook_output_observe`。
/// 发送与观察各自独立拿锁，不持锁跨 tx.send（比归档更安全，避免死锁）。
///
/// observe 钩子按注册顺序串行执行，单个 panic 或超时不阻塞后续（见 HooksRegistry）。
///
/// 暴露为 `pub(crate)` 供工具调用等分离式场景在拦截 + 处理后单独调用。
pub(crate) async fn deliver(emitter: &Emitter, hooks: &SharedHooks, event: OutputEvent) {
    // observe 需要拿到与发送一致的事件，先 clone 一份留给 observe
    let observe_event = event.clone();

    // 先发送（Emitter 负责：盖 session_id 标签 + 推到出口通道）
    emitter.emit(event).await;

    // 再观察（独立拿锁，不持锁跨 tx.send）
    let hooks = hooks.lock().await;
    hooks.hook_output_observe(observe_event).await;
}
