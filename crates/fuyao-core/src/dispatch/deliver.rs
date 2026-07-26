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
///
/// 返回原 event（move 进来再还回去）——`emit` 按值消费 event，本函数在 emit 前
/// 先 clone 一份给 observe 用，emit 完成后把这份 clone 还给调用方，让调用方
/// （如 `emit_to_history`）不必再为返回值单独 clone 一次。
pub(crate) async fn deliver(
    emitter: &Emitter,
    hooks: &SharedHooks,
    event: OutputEvent,
) -> OutputEvent {
    // observe 需要拿到与发送一致的事件，先 clone 一份留给 observe；
    // 这份 clone 同时也是返回值——emit 之后 event 已 move，observe_event 是唯一剩余副本
    let observe_event = event.clone();

    // 先发送（Emitter 负责：盖 session_id 标签 + 推到出口通道）
    // 出站通道无界，emit 同步返回——但本函数仍保留 async 因 observe hook 可能跨 await
    emitter.emit(event);

    // 再观察（独立拿锁，不持锁跨 tx.send）
    let hooks = hooks.lock().await;
    hooks.hook_output_observe(observe_event.clone()).await;

    observe_event
}
