//! 拦截步骤
//!
//! 调用 `hook_output_intercept`，插件可修改或阻断事件。
//! Block 时返回 None（调用方放弃后续处理），Pass 时返回可能被修改的事件。

use crate::emit::Emitter;
use fuyao_api::message::OutputEvent;
use fuyao_hooks::{InterceptResult, SharedHooks};

/// 执行拦截钩子
///
/// 返回 `Some(event)` 表示 Pass（事件可能被插件修改）；
/// 返回 `None` 表示 Block，调用方应丢弃该事件。
///
/// 锁仅在拦截期间持有，不跨 await 边界（拦截是同步调用）。
pub(crate) async fn intercept(
    _emitter: &Emitter,
    hooks: &SharedHooks,
    event: OutputEvent,
) -> Option<OutputEvent> {
    let hooks = hooks.lock().await;
    match hooks.hook_output_intercept(&event) {
        InterceptResult::Pass(modified) => Some(modified),
        InterceptResult::Block(reason) => {
            tracing::warn!(
                hook = "output_intercept",
                block = true,
                reason = %reason,
                "事件被拦截钩子丢弃"
            );
            None
        }
    }
}
