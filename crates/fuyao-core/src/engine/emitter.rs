//! 统一事件发送器
//!
//! 提供拦截和投递两个独立操作，由 dispatch 管道组合使用：
//! - intercept：插件拦截/修改/阻断
//! - deliver：发送到 CLI 渲染通道 + 插件观察
//!
//! send_input 钩子是真·主动模式，由引擎启动时 init_send_inputs 初始化，
//! 插件持有 Sender 可随时发送，不依赖调用频率。

use crate::engine::types::SharedHooks;
use fuyao_api::message::OutputEvent;
use tokio::sync::mpsc;

#[derive(Clone)]
pub(crate) struct EventEmitter {
    /// 输出事件发送端
    tx_event: mpsc::Sender<OutputEvent>,
    /// 共享钩子注册表
    hooks: SharedHooks,
}

impl EventEmitter {
    pub fn new(tx_event: mpsc::Sender<OutputEvent>, hooks: SharedHooks) -> Self {
        Self { tx_event, hooks }
    }

    /// 发送事件到 CLI 渲染通道
    pub async fn send(&self, event: OutputEvent) {
        if self.tx_event.send(event).await.is_err() {
            tracing::warn!("输出事件发送失败，输出通道已关闭");
        }
    }

    /// 获取 hooks 引用（供 dispatch 管道和 Engine 使用）
    pub fn hooks(&self) -> &SharedHooks {
        &self.hooks
    }
}
