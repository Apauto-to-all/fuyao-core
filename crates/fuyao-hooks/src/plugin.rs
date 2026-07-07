//! 插件抽象
//!
//! Plugin trait 统一 hook 注册身份；PluginHost 收集插件并统一装配/清理。
//! 执行引擎（HooksRegistry）不变，Plugin 只是"register_xxx_hooks 函数"的 trait 化包装。

use std::panic::AssertUnwindSafe;

use futures_util::future::FutureExt;
use fuyao_api::message::EventBase;
use fuyao_api::message::input::{InputEvent, PluginEventSource, PluginMessage, PluginPayload};
use tokio::sync::mpsc::Sender;

use crate::SharedHooks;

/// 插件 trait：统一的 hook 注册身份
///
/// 插件通过 [`register`](Plugin::register) 把闭包注册到 HooksRegistry，
/// 通过 [`dispose`](Plugin::dispose) 清理资源。
///
/// **不接收工具/上下文参数** —— 需要额外上下文的插件在构造时持有它们。
/// 工具注册（register_builtin_tools / register_mcp_tools）不纳入 Plugin trait。
#[async_trait::async_trait]
pub trait Plugin: Send + Sync {
    /// 插件唯一标识（调试、日志、配置开关匹配用）
    fn name(&self) -> &str;

    /// 插件结构化身份（默认从 name() 桥接）
    ///
    /// 返回 [`PluginEventSource`]，用作 [`PluginEmitter`] 的身份绑定。
    /// 默认实现桥接 `name()`，现有插件零改动；未来需要更丰富身份（version 等）可 override。
    fn identity(&self) -> PluginEventSource {
        PluginEventSource {
            name: self.name().to_string(),
        }
    }

    /// 注册阶段：拿到 SharedHooks，自行调用 register_before_llm /
    /// register_output_intercept / register_output_observe /
    /// register_on_llm_error / register_send_input 中的任意组合。
    ///
    /// 默认空实现（观察型插件可不注册任何 hook）。
    async fn register(&self, _hooks: &SharedHooks) {}

    /// 销毁阶段：引擎卸载时调用，用于清理资源（关闭连接、刷盘等）。
    ///
    /// 默认空实现。
    async fn dispose(&self) {}
}

/// 插件消息发送器：绑定身份，自动填充 source
///
/// identity 在构造时绑定（应来自 [`Plugin::identity`]），之后私有不可改。
/// 插件持有 emitter 后，发 Plugin 消息无需手填 name，杜绝命名漂移。
/// [`sender`](Self::sender) 暴露原始通道，供发 Interrupt/User 等非 Plugin 消息复用。
pub struct PluginEmitter {
    /// 绑定的插件身份（私有，构造后不可改）
    identity: PluginEventSource,
    /// 引擎输入通道
    tx: Sender<InputEvent>,
}

impl PluginEmitter {
    /// 构造：identity 应来自 [`Plugin::identity`]，tx 来自 send_input 钩子回调
    pub fn new(identity: PluginEventSource, tx: Sender<InputEvent>) -> Self {
        Self { identity, tx }
    }

    /// 发送完整插件事件（自动填 source = identity, base = default）
    ///
    /// 发送失败（通道满/关闭）静默忽略，不阻塞 hook 执行。
    pub fn emit(
        &self,
        event_type: &str,
        data: Option<serde_json::Value>,
        error: Option<String>,
        message: Option<String>,
    ) {
        let _ = self.tx.try_send(InputEvent::Plugin(PluginMessage {
            base: EventBase::default(),
            payload: PluginPayload {
                source: self.identity.clone(),
                event_type: event_type.to_string(),
                data,
                error,
                message,
            },
        }));
    }

    /// 便捷：发送 message 通知（最常见的场景）
    pub fn emit_message(&self, event_type: &str, message: &str) {
        self.emit(event_type, None, None, Some(message.to_string()));
    }

    /// 便捷：发送带 data 的事件（统计、进度等）
    pub fn emit_data(&self, event_type: &str, data: serde_json::Value) {
        self.emit(event_type, Some(data), None, None);
    }

    /// 身份（只读，供调试 / 日志 / 构造其他来源标识复用）
    pub fn identity(&self) -> &PluginEventSource {
        &self.identity
    }

    /// 输入通道引用（供发送 Interrupt/User 等非 Plugin 消息复用）
    pub fn sender(&self) -> &Sender<InputEvent> {
        &self.tx
    }
}

/// 插件错误
///
/// install / dispose_all 内部捕获 panic 后构造此类型记录日志，**不向上抛**。
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// register 阶段失败（含 panic 转译）
    #[error("插件 register 失败: {0}")]
    Register(String),
    /// dispose 阶段失败
    #[error("插件 dispose 失败: {0}")]
    Dispose(String),
}

/// 插件装配错误（install 阶段）
#[derive(Debug, thiserror::Error)]
pub enum PluginInstallError {
    /// 插件重名：core 不接受任何重名情况
    #[error("插件重名: {name}")]
    DuplicateName { name: String },
}

/// 把 panic payload（`Box<dyn Any + Send>`）转为可读 String
fn panic_payload_to_string(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "未知 panic".to_string()
    }
}

/// 插件装配宿主
///
/// 收集插件，统一调用 [`register`](Plugin::register)（装配阶段）和
/// [`dispose`](Plugin::dispose)（清理阶段）。
///
/// 不持有 SharedHooks —— 由调用方在 [`install`](Self::install) 时传入，
/// 避免 host 与引擎生命周期绑定。
pub struct PluginHost {
    plugins: Vec<Box<dyn Plugin>>,
}

impl Default for PluginHost {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginHost {
    /// 创建空主机
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
        }
    }

    /// 添加插件（注册顺序 = 执行顺序）
    pub fn add(&mut self, plugin: Box<dyn Plugin>) {
        self.plugins.push(plugin);
    }

    /// 装配：遍历所有插件调用 register。
    ///
    /// - **重名硬失败**：core 不接受任何重名，检测到重名立即返回 `Err`
    /// - **register panic 防护**：单个插件 register 崩溃不阻塞其他插件（eprintln + 继续）
    pub async fn install(&self, hooks: &SharedHooks) -> Result<(), PluginInstallError> {
        let mut seen = std::collections::HashSet::new();
        for plugin in &self.plugins {
            let name = plugin.name().to_string();
            // 唯一性校验：core 不接受任何重名
            if !seen.insert(name.clone()) {
                return Err(PluginInstallError::DuplicateName { name });
            }
            match AssertUnwindSafe(plugin.register(hooks))
                .catch_unwind()
                .await
            {
                Ok(()) => {}
                Err(payload) => {
                    eprintln!(
                        "{}",
                        PluginError::Register(format!(
                            "插件 {name}: {}",
                            panic_payload_to_string(&*payload)
                        ))
                    );
                }
            }
        }
        Ok(())
    }

    /// 遍历所有插件调用 dispose（逆序 LIFO）。单个失败不阻塞。
    pub async fn dispose_all(&self) {
        for plugin in self.plugins.iter().rev() {
            let name = plugin.name().to_string();
            match AssertUnwindSafe(plugin.dispose()).catch_unwind().await {
                Ok(()) => {}
                Err(payload) => {
                    eprintln!(
                        "{}",
                        PluginError::Dispose(format!(
                            "插件 {name}: {}",
                            panic_payload_to_string(&*payload)
                        ))
                    );
                }
            }
        }
    }

    /// 当前插件名称列表（调试用）
    pub fn list(&self) -> Vec<&str> {
        self.plugins.iter().map(|p| p.name()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 测试插件：记录 register / dispose 调用次数
    struct CountingPlugin {
        name: &'static str,
        register_count: Arc<AtomicUsize>,
        dispose_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Plugin for CountingPlugin {
        fn name(&self) -> &str {
            self.name
        }
        async fn register(&self, _hooks: &SharedHooks) {
            self.register_count.fetch_add(1, Ordering::SeqCst);
        }
        async fn dispose(&self) {
            self.dispose_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn empty_hooks() -> SharedHooks {
        Arc::new(tokio::sync::Mutex::new(crate::HooksRegistry::new()))
    }

    #[tokio::test]
    async fn install_calls_register_for_all_plugins() {
        let reg_a = Arc::new(AtomicUsize::new(0));
        let reg_b = Arc::new(AtomicUsize::new(0));

        let mut host = PluginHost::new();
        host.add(Box::new(CountingPlugin {
            name: "a",
            register_count: reg_a.clone(),
            dispose_count: Arc::new(AtomicUsize::new(0)),
        }));
        host.add(Box::new(CountingPlugin {
            name: "b",
            register_count: reg_b.clone(),
            dispose_count: Arc::new(AtomicUsize::new(0)),
        }));

        host.install(&empty_hooks()).await.unwrap();

        assert_eq!(reg_a.load(Ordering::SeqCst), 1);
        assert_eq!(reg_b.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dispose_all_calls_in_reverse_order() {
        let order = Arc::new(std::sync::Mutex::new(Vec::<&str>::new()));

        struct OrderedPlugin {
            name: &'static str,
            order: Arc<std::sync::Mutex<Vec<&'static str>>>,
        }

        #[async_trait::async_trait]
        impl Plugin for OrderedPlugin {
            fn name(&self) -> &str {
                self.name
            }
            async fn dispose(&self) {
                self.order.lock().unwrap().push(self.name);
            }
        }

        let mut host = PluginHost::new();
        host.add(Box::new(OrderedPlugin {
            name: "first",
            order: order.clone(),
        }));
        host.add(Box::new(OrderedPlugin {
            name: "second",
            order: order.clone(),
        }));
        host.add(Box::new(OrderedPlugin {
            name: "third",
            order: order.clone(),
        }));

        host.dispose_all().await;

        let order = order.lock().unwrap();
        assert_eq!(*order, vec!["third", "second", "first"]);
    }

    #[tokio::test]
    async fn install_panic_protection() {
        struct PanicPlugin;
        #[async_trait::async_trait]
        impl Plugin for PanicPlugin {
            fn name(&self) -> &str {
                "panic"
            }
            async fn register(&self, _hooks: &SharedHooks) {
                panic!("register 崩溃");
            }
        }

        let called = Arc::new(AtomicUsize::new(0));

        struct NormalPlugin {
            called: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl Plugin for NormalPlugin {
            fn name(&self) -> &str {
                "normal"
            }
            async fn register(&self, _hooks: &SharedHooks) {
                self.called.fetch_add(1, Ordering::SeqCst);
            }
        }

        let mut host = PluginHost::new();
        host.add(Box::new(PanicPlugin));
        host.add(Box::new(NormalPlugin {
            called: called.clone(),
        }));

        host.install(&empty_hooks()).await.unwrap();

        // panic 插件不阻塞正常插件
        assert_eq!(called.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dispose_all_panic_protection() {
        struct PanicDispose;
        #[async_trait::async_trait]
        impl Plugin for PanicDispose {
            fn name(&self) -> &str {
                "panic_dispose"
            }
            async fn dispose(&self) {
                panic!("dispose 崩溃");
            }
        }

        let called = Arc::new(AtomicUsize::new(0));

        struct NormalDispose {
            called: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl Plugin for NormalDispose {
            fn name(&self) -> &str {
                "normal_dispose"
            }
            async fn dispose(&self) {
                self.called.fetch_add(1, Ordering::SeqCst);
            }
        }

        let mut host = PluginHost::new();
        host.add(Box::new(NormalDispose {
            called: called.clone(),
        }));
        host.add(Box::new(PanicDispose));

        host.dispose_all().await;

        // 逆序：PanicDispose 先 dispose（panic 被捕获），NormalDispose 后 dispose（正常）
        assert_eq!(called.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn list_returns_plugin_names() {
        let mut host = PluginHost::new();
        host.add(Box::new(CountingPlugin {
            name: "a",
            register_count: Arc::new(AtomicUsize::new(0)),
            dispose_count: Arc::new(AtomicUsize::new(0)),
        }));
        host.add(Box::new(CountingPlugin {
            name: "b",
            register_count: Arc::new(AtomicUsize::new(0)),
            dispose_count: Arc::new(AtomicUsize::new(0)),
        }));

        assert_eq!(host.list(), vec!["a", "b"]);
    }

    /// identity() 默认从 name() 桥接
    #[test]
    fn identity_defaults_from_name() {
        let plugin = CountingPlugin {
            name: "my_plugin",
            register_count: Arc::new(AtomicUsize::new(0)),
            dispose_count: Arc::new(AtomicUsize::new(0)),
        };
        assert_eq!(plugin.identity().name, "my_plugin");
    }

    /// 重名插件装配失败（core 不接受任何重名）
    #[tokio::test]
    async fn install_rejects_duplicate_names() {
        let mut host = PluginHost::new();
        host.add(Box::new(CountingPlugin {
            name: "dup",
            register_count: Arc::new(AtomicUsize::new(0)),
            dispose_count: Arc::new(AtomicUsize::new(0)),
        }));
        host.add(Box::new(CountingPlugin {
            name: "dup",
            register_count: Arc::new(AtomicUsize::new(0)),
            dispose_count: Arc::new(AtomicUsize::new(0)),
        }));

        let result = host.install(&empty_hooks()).await;
        assert!(matches!(
            result,
            Err(PluginInstallError::DuplicateName { ref name }) if name == "dup"
        ));
    }

    /// PluginEmitter emit_message 后消息 source = 构造时的 identity
    #[tokio::test]
    async fn emitter_emit_message_fills_source() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let emitter = PluginEmitter::new(
            PluginEventSource {
                name: "test_plugin".into(),
            },
            tx,
        );
        emitter.emit_message("warn", "检测到异常");

        let event = rx.try_recv().unwrap();
        match event {
            InputEvent::Plugin(data) => {
                assert_eq!(data.payload.source.name, "test_plugin");
                assert_eq!(data.payload.event_type, "warn");
                assert_eq!(data.payload.message, Some("检测到异常".to_string()));
            }
            _ => panic!("应为 Plugin 事件"),
        }
    }

    /// PluginEmitter emit_data 带 data 字段
    #[tokio::test]
    async fn emitter_emit_data_carries_payload() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let emitter = PluginEmitter::new(
            PluginEventSource {
                name: "stats".into(),
            },
            tx,
        );
        emitter.emit_data("cumulative", serde_json::json!({"count": 42}));

        let event = rx.try_recv().unwrap();
        match event {
            InputEvent::Plugin(data) => {
                assert_eq!(data.payload.source.name, "stats");
                assert_eq!(data.payload.data.unwrap()["count"], 42);
            }
            _ => panic!("应为 Plugin 事件"),
        }
    }

    /// PluginEmitter identity 只读，sender 暴露通道
    #[tokio::test]
    async fn emitter_identity_and_sender() {
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let emitter = PluginEmitter::new(PluginEventSource { name: "p".into() }, tx);
        assert_eq!(emitter.identity().name, "p");
        // sender 可用（编译期验证 + 发送 Interrupt 消息复用通道）
        let _ = emitter.sender().try_send(InputEvent::Shutdown(
            fuyao_api::message::input::ShutdownMessage {
                base: EventBase::default(),
            },
        ));
    }
}
