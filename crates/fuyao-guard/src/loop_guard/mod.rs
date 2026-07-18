//! 循环检测插件
//!
//! 通过 observe/intercept 钩子检测 AI 输出重复和工具循环。
//! 当检测到循环时，根据严重程度采取不同措施（警告、注入结果或终止）。
//! 发消息能力通过 send_input hook 拿到 SessionSender（统一原则：插件一切能力都是 hook）。
//!
//! 两层模型：
//! - [`LoopGuardPlugin`]（工厂）：引擎级，持 `LoopGuardConfig`；每 session 调用
//!   `create_instance` 生成独立实例
//! - [`LoopGuardInstance`]（实例）：session 级，持独立的 `LoopGuardState`，
//!   register 时注册 observe/intercept/send_input 三个钩子

mod detectors;
mod guard;
mod text_guard;
mod tool_guard;
pub mod types;

use std::sync::Arc;

use fuyao_hooks::{HooksRegistry, Plugin, PluginInstance, SendInputFn, SessionSender};
use tokio::sync::Mutex;

use fuyao_api::LoopGuardConfig;
use guard::{LoopGuardState, make_output_intercept, make_output_observe};

/// 循环检测插件（工厂模板，引擎级）
///
/// 通过 output_observe（检测累积）+ output_intercept（注入警告/替换内容）+
/// send_input（获取 SessionSender）三个钩子实现循环检测。
///
/// 引擎级只持有配置；每 session 启动时 [`create_instance`](Plugin::create_instance)
/// 新建独立 `LoopGuardState`，多 session 并发互不串台。
///
/// 配置从全局句柄 `get_config().guard.loop_` 读取（由应用装配层 `init_engine` / `set_config` 注入）；
/// 未注入时回退 `LoopGuardConfig::default()`。
pub struct LoopGuardPlugin {
    config: LoopGuardConfig,
}

impl LoopGuardPlugin {
    /// 从全局配置构造循环检测插件
    pub fn new() -> Self {
        Self {
            config: fuyao_api::get_config().guard.loop_.clone(),
        }
    }

    /// 用指定配置构造（测试场景或装配方覆盖配置时用）
    pub fn with_config(config: LoopGuardConfig) -> Self {
        Self { config }
    }
}

impl Default for LoopGuardPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for LoopGuardPlugin {
    fn name(&self) -> &str {
        "loop_guard"
    }

    fn create_instance(&self) -> Box<dyn PluginInstance> {
        // 每 session 新建独立 state（多 session 并发时互不串台）
        Box::new(LoopGuardInstance {
            state: Arc::new(Mutex::new(LoopGuardState::new(self.config.clone()))),
        })
    }
}

/// 循环检测插件实例（session 级）
///
/// 持该 session 独立的 `LoopGuardState`，register 时注册三个钩子：
/// - `output_observe`：累积检测（chunk 文本 + tool_call 历史 + 用户消息重置时机）
/// - `output_intercept`：注入警告或替换工具结果内容
/// - `send_input`：拿到 SessionSender 后调 `set_sender` 保存到 state
struct LoopGuardInstance {
    state: Arc<Mutex<LoopGuardState>>,
}

impl PluginInstance for LoopGuardInstance {
    fn register(&self, hooks: &mut HooksRegistry) {
        let observe = make_output_observe(self.state.clone());
        let intercept = make_output_intercept(self.state.clone());

        // send_input：拿到 SessionSender 后注入 state，
        // 后续 emit_plugin / send_interrupt / send_inject_message 经它发消息
        let send_state = self.state.clone();
        let send_fn: SendInputFn = Arc::new(move |sender: SessionSender| {
            let s = send_state.clone();
            Box::pin(async move {
                s.lock().await.set_sender(sender);
            })
        });

        hooks.register_output_observe(observe);
        hooks.register_output_intercept(10, intercept);
        hooks.register_send_input(0, send_fn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证插件构造 + create_instance 不 panic
    #[test]
    fn plugin_construct_and_create_instance() {
        let plugin = LoopGuardPlugin::with_config(LoopGuardConfig::default());
        assert_eq!(plugin.name(), "loop_guard");

        // create_instance 不 panic 即说明工厂模板可生产实例
        let instance = plugin.create_instance();
        let mut hooks = HooksRegistry::new();
        // register 不 panic 即说明三个钩子注册成功
        instance.register(&mut hooks);
    }
}
