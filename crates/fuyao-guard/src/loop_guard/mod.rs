//! 循环检测插件
//!
//! 通过 observe/intercept 钩子检测 AI 输出重复和工具循环。
//! 当检测到循环时，根据严重程度采取不同措施（警告、注入结果或终止）。
//! 所有事件发送通过 SendInputFn 获取的 tx_send 实现，
//! 不持有引擎内部 channel。

mod detectors;
mod guard;
mod text_guard;
mod tool_guard;
pub mod types;

use std::sync::Arc;

use fuyao_api::LoopGuardConfig;
use fuyao_hooks::HooksRegistry;
use tokio::sync::Mutex;

use guard::{LoopGuardState, make_output_intercept, make_output_observe};

/// 注册循环检测钩子
///
/// 在 HooksRegistry 中注册 output_observe、output_intercept 和 send_input 钩子。
/// 配置从全局句柄 `get_config().guard.loop` 读取（由 `init_engine` 注入）；
/// 未注入时回退 `LoopGuardConfig::default()`。
///
/// # 参数
/// - `hooks`: 钩子注册表
pub async fn register_loop_guard_hooks(hooks: &Arc<Mutex<HooksRegistry>>) {
    let config = fuyao_api::get_config().guard.loop_.clone();
    register_loop_guard_hooks_with_config(hooks, config).await;
}

/// 注册循环检测钩子（自定义配置）
pub async fn register_loop_guard_hooks_with_config(
    hooks: &Arc<Mutex<HooksRegistry>>,
    config: LoopGuardConfig,
) {
    let state = Arc::new(Mutex::new(LoopGuardState::new(config)));

    let observe = make_output_observe(state.clone());
    let intercept = make_output_intercept(state.clone());

    // send_input：保存 tx_send 到 state，后续用于发送中断、注入消息、插件通知
    let send_state = state.clone();
    let send_fn: fuyao_hooks::SendInputFn = Arc::new(move |tx| {
        let s = send_state.clone();
        Box::pin(async move {
            s.lock().await.set_tx_send(tx);
        })
    });

    let mut h = hooks.lock().await;
    h.register_output_observe(observe);
    h.register_output_intercept(10, intercept);
    h.register_send_input(0, send_fn);
}
