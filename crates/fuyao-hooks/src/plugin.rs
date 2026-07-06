//! 插件抽象
//!
//! Plugin trait 统一 hook 注册身份；PluginHost 收集插件并统一装配/清理。
//! 执行引擎（HooksRegistry）不变，Plugin 只是"register_xxx_hooks 函数"的 trait 化包装。

use std::panic::AssertUnwindSafe;

use futures_util::future::FutureExt;

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

    /// 遍历所有插件调用 register。单个插件 panic 不阻塞其他插件。
    pub async fn install(&self, hooks: &SharedHooks) {
        for plugin in &self.plugins {
            let name = plugin.name().to_string();
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

        host.install(&empty_hooks()).await;

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

        host.install(&empty_hooks()).await;

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
}
