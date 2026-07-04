//! 消息分发管道
//!
//! 统一所有输入事件到输出事件的处理链路：
//! 转化 → 拦截 → 处理回调 → 发送 → 观察
//!
//! 每个步骤独立文件，便于维护和拓展管道顺序。
//!
//! 提供两种调用方式：
//! - `dispatch()`：一气呵成（拦截 → 处理 → 发送 → 观察）
//! - `dispatch_intercept()`：仅拦截 + 处理，返回拦截后事件；
//!   调用方在合适时机调 `deliver::run` 自行 deliver（用于 User 消息等延迟发送场景）

mod deliver;
mod intercept;
mod process;

use crate::engine::EventEmitter;
use fuyao_api::message::OutputEvent;
use fuyao_hooks::InterceptResult;

// 暴露 deliver::run 给延迟 deliver 场景（如 TurnExecutor 消费队列时）调用
pub(crate) use deliver::run as deliver;

/// 处理回调类型
///
/// 在拦截之后、发送之前执行的引擎内部业务逻辑。
/// 例如：发送命令给 TurnExecutor。
pub type ProcessCallback = Box<dyn FnOnce() + Send>;

/// 拦截 + 处理回调（不含 deliver）
///
/// 返回 `Some(event)` 表示拦截器 Pass（可能被修改）；
/// 返回 `None` 表示拦截器 Block，调用方应放弃后续处理。
///
/// 调用方拿到拦截后的事件后，在合适时机调用 `deliver::run` 完成发送+观察。
pub(crate) async fn dispatch_intercept(
    event: OutputEvent,
    process_fn: Option<ProcessCallback>,
    emitter: &EventEmitter,
) -> Option<OutputEvent> {
    // 1. 拦截
    let event = match intercept::run(emitter, event).await {
        InterceptResult::Pass(e) => e,
        InterceptResult::Block(_) => return None,
    };

    // 2. 处理回调
    if let Some(callback) = process_fn {
        process::run(callback);
    }

    Some(event)
}

/// 统一分发管道（一气呵成）
///
/// 将输入事件转化为输出事件后，依次执行：
/// 1. 拦截：插件可修改或阻断事件
/// 2. 处理回调：引擎内部业务逻辑
/// 3. 发送：推送到 CLI 渲染通道
/// 4. 观察：插件存储/统计/通知
pub(crate) async fn dispatch(
    event: OutputEvent,
    process_fn: Option<ProcessCallback>,
    emitter: &EventEmitter,
) {
    // 拦截 + 处理
    let event = match dispatch_intercept(event, process_fn, emitter).await {
        Some(e) => e,
        None => return,
    };

    // 3. 发送 + 观察
    deliver::run(emitter, event).await;
}
