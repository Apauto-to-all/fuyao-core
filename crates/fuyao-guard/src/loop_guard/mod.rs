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

use fuyao_hooks::{Plugin, SharedHooks};
use tokio::sync::Mutex;

use guard::{LoopGuardState, make_output_intercept, make_output_observe};

/// 循环检测插件
///
/// 通过 output_observe（检测累积）+ output_intercept（注入警告/替换内容）+
/// send_input（获取 tx_send）三个钩子实现循环检测。
///
/// 配置从全局句柄 `get_config().guard.loop_` 读取（由应用装配层 `init_engine` / `set_config` 注入）；
/// 未注入时回退 `LoopGuardConfig::default()`。
pub struct LoopGuardPlugin {
    state: Arc<Mutex<LoopGuardState>>,
}

impl LoopGuardPlugin {
    /// 从全局配置构造循环检测插件
    pub fn new() -> Self {
        let config = fuyao_api::get_config().guard.loop_.clone();
        Self {
            state: Arc::new(Mutex::new(LoopGuardState::new(config))),
        }
    }
}

impl Default for LoopGuardPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Plugin for LoopGuardPlugin {
    fn name(&self) -> &str {
        "loop_guard"
    }

    async fn register(&self, hooks: &SharedHooks) {
        let observe = make_output_observe(self.state.clone());
        let intercept = make_output_intercept(self.state.clone());

        // send_input：保存 tx_send 到 state，后续用于发送中断、注入消息、插件通知
        let send_state = self.state.clone();
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证插件构造 + 注册不 panic
    #[tokio::test]
    async fn plugin_construct_and_register() {
        let plugin = LoopGuardPlugin::new();
        assert_eq!(plugin.name(), "loop_guard");

        let hooks: SharedHooks =
            Arc::new(tokio::sync::Mutex::new(fuyao_hooks::HooksRegistry::new()));
        // register 不 panic 即说明三个钩子注册成功
        plugin.register(&hooks).await;
    }
}
