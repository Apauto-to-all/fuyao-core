//! fuyao-hooks 集成测试共享 fixture
//!
//! 手写 fake plugin 与钩子闭包构造辅助。源码内的 fake plugin 在 #[cfg(test)] 内，
//! 集成测试（外部 crate）不可见，故在此重新定义可复用的测试替身。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use fuyao_hooks::{BeforeLlmOutput, Plugin, SharedHooks};

// ---------------------------------------------------------------------------
// 计数型 fake plugin：记录 register / dispose 调用次数
// ---------------------------------------------------------------------------

/// 记录 register 与 dispose 次数的 fake plugin，用于验证装配与清理链路。
pub struct CountingPlugin {
    name: &'static str,
    pub register_count: Arc<AtomicUsize>,
    pub dispose_count: Arc<AtomicUsize>,
}

impl CountingPlugin {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            register_count: Arc::new(AtomicUsize::new(0)),
            dispose_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
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

// ---------------------------------------------------------------------------
// 逆序记录型 fake plugin：dispose 时把 name 推入共享 vec，验证 LIFO
// ---------------------------------------------------------------------------

/// dispose 时把 name 记录到共享 vec，用于验证 dispose 的逆序（LIFO）语义。
pub struct OrderedPlugin {
    name: &'static str,
    pub dispose_order: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

impl OrderedPlugin {
    pub fn new(
        name: &'static str,
        dispose_order: Arc<std::sync::Mutex<Vec<&'static str>>>,
    ) -> Self {
        Self {
            name,
            dispose_order,
        }
    }
}

#[async_trait]
impl Plugin for OrderedPlugin {
    fn name(&self) -> &str {
        self.name
    }
    async fn dispose(&self) {
        self.dispose_order.lock().unwrap().push(self.name);
    }
}

// ---------------------------------------------------------------------------
// register 时 panic 的 fake plugin：验证 panic 防护不阻塞后续插件
// ---------------------------------------------------------------------------

pub struct PanicOnRegisterPlugin {
    name: &'static str,
}

impl PanicOnRegisterPlugin {
    pub fn new(name: &'static str) -> Self {
        Self { name }
    }
}

#[async_trait]
impl Plugin for PanicOnRegisterPlugin {
    fn name(&self) -> &str {
        self.name
    }
    async fn register(&self, _hooks: &SharedHooks) {
        panic!("register 故意崩溃");
    }
}

// ---------------------------------------------------------------------------
// 注册 output_observe 钩子的 fake plugin：验证装配后 hook 真正触发
// ---------------------------------------------------------------------------

use fuyao_api::message::OutputEvent;
use fuyao_hooks::OutputObserveFn;

/// 注册一个 output_observe 钩子，钩子收到事件时计数 +1。
pub struct ObservePlugin {
    name: &'static str,
    pub observe_count: Arc<AtomicUsize>,
}

impl ObservePlugin {
    pub fn new(name: &'static str, observe_count: Arc<AtomicUsize>) -> Self {
        Self {
            name,
            observe_count,
        }
    }
}

#[async_trait]
impl Plugin for ObservePlugin {
    fn name(&self) -> &str {
        self.name
    }
    async fn register(&self, hooks: &SharedHooks) {
        let counter = self.observe_count.clone();
        let handler: OutputObserveFn = Arc::new(move |_msg: OutputEvent| {
            let c = counter.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        });
        hooks.lock().await.register_output_observe(handler);
    }
}

// ---------------------------------------------------------------------------
// 注册 output_intercept 拦截钩子的 fake plugin：验证 Block 短路
// ---------------------------------------------------------------------------

use fuyao_hooks::{InterceptResult, OutputInterceptFn};

/// 注册一个 output_intercept 钩子，返回 Block 或 Pass。
pub struct InterceptPlugin {
    name: &'static str,
    block: bool,
}

impl InterceptPlugin {
    pub fn new(name: &'static str, block: bool) -> Self {
        Self { name, block }
    }
}

#[async_trait]
impl Plugin for InterceptPlugin {
    fn name(&self) -> &str {
        self.name
    }
    async fn register(&self, hooks: &SharedHooks) {
        let block = self.block;
        let handler: OutputInterceptFn = Arc::new(move |msg: &OutputEvent| {
            if block {
                InterceptResult::Block("插件拦截".to_string())
            } else {
                InterceptResult::Pass(msg.clone())
            }
        });
        hooks.lock().await.register_output_intercept(0, handler);
    }
}

// ---------------------------------------------------------------------------
// 注册 before_llm 钩子的 fake plugin：验证 OR 语义
// ---------------------------------------------------------------------------

use fuyao_api::Message;
use fuyao_hooks::BeforeLlmFn;

/// 注册 before_llm 钩子，可控制返回的 messages 与 skip_tools。
pub struct BeforeLlmPlugin {
    name: &'static str,
    inject_messages: Vec<Message>,
    skip_tools: bool,
}

impl BeforeLlmPlugin {
    pub fn new(name: &'static str, inject_messages: Vec<Message>, skip_tools: bool) -> Self {
        Self {
            name,
            inject_messages,
            skip_tools,
        }
    }
}

#[async_trait]
impl Plugin for BeforeLlmPlugin {
    fn name(&self) -> &str {
        self.name
    }
    async fn register(&self, hooks: &SharedHooks) {
        // BeforeLlmOutput 未派生 Clone，闭包内每次构造新实例
        let messages = self.inject_messages.clone();
        let skip = self.skip_tools;
        let handler: BeforeLlmFn = Arc::new(move || {
            let msgs = messages.clone();
            Box::pin(async move {
                BeforeLlmOutput {
                    messages: msgs,
                    skip_tools: skip,
                }
            })
        });
        hooks.lock().await.register_before_llm(0, handler);
    }
}

/// 构造空 SharedHooks（Arc<Mutex<HooksRegistry>>）
#[allow(dead_code)]
pub fn empty_hooks() -> SharedHooks {
    Arc::new(tokio::sync::Mutex::new(fuyao_hooks::HooksRegistry::new()))
}
