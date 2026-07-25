//! plugin 模块单元测试
//!
//! 覆盖：
//! - PluginHost（create_instances / dispose_all / validate_unique_names / list / panic 防护）
//! - Plugin trait 默认 identity 实现
//! - PluginInstance trait register 行为
//! - SessionSender 三通道分流 + 身份绑定

use super::factory::Plugin;
use super::host::{PluginHost, PluginInstallError};
use super::instance::PluginInstance;
use super::sender::SessionSender;
use crate::HooksRegistry;
use fuyao_api::UserMessageMode;
use fuyao_api::message::input::{
    InterruptMessage, InterruptSource, PluginEventSource, PluginMessage,
};
use fuyao_api::message::output::UserMessage as OutputUserMessage;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// 测试辅助：fake plugin / fake instance
// ---------------------------------------------------------------------------

/// 记录 register / dispose 调用次数的 fake instance
struct CountingInstance {
    register_count: Arc<AtomicUsize>,
    dispose_count: Arc<AtomicUsize>,
}

impl PluginInstance for CountingInstance {
    fn register(&self, _hooks: &mut HooksRegistry) {
        self.register_count.fetch_add(1, Ordering::SeqCst);
    }
    fn dispose(&self) {
        self.dispose_count.fetch_add(1, Ordering::SeqCst);
    }
}

/// 工厂：每次 create_instance 生成一个独立 CountingInstance
struct CountingPlugin {
    name: &'static str,
    register_count: Arc<AtomicUsize>,
    dispose_count: Arc<AtomicUsize>,
    instance_count: Arc<AtomicUsize>,
}

impl Plugin for CountingPlugin {
    fn name(&self) -> &str {
        self.name
    }
    fn create_instance(&self) -> Box<dyn PluginInstance> {
        self.instance_count.fetch_add(1, Ordering::SeqCst);
        Box::new(CountingInstance {
            register_count: self.register_count.clone(),
            dispose_count: self.dispose_count.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// PluginHost：create_instances
// ---------------------------------------------------------------------------

/// create_instances 为每个插件生成一个实例
#[test]
fn create_instances_returns_one_instance_per_plugin() {
    let reg = Arc::new(AtomicUsize::new(0));
    let disp = Arc::new(AtomicUsize::new(0));
    let inst = Arc::new(AtomicUsize::new(0));

    let mut host = PluginHost::new();
    host.add(Box::new(CountingPlugin {
        name: "a",
        register_count: reg.clone(),
        dispose_count: disp.clone(),
        instance_count: inst.clone(),
    }));
    host.add(Box::new(CountingPlugin {
        name: "b",
        register_count: reg.clone(),
        dispose_count: disp.clone(),
        instance_count: inst.clone(),
    }));

    let instances = host.create_instances().unwrap();
    assert_eq!(instances.len(), 2, "应为 2 个插件各生成 1 个实例");
    assert_eq!(
        inst.load(Ordering::SeqCst),
        2,
        "create_instance 应被调用 2 次"
    );

    // 实例的 register 尚未被调用（要由调用方逐个调）
    assert_eq!(reg.load(Ordering::SeqCst), 0, "register 不应被自动调用");
}

/// 每个实例的 register 被调用时计数 +1（验证调用方流程）
#[test]
fn instance_register_called_when_invoked() {
    let reg = Arc::new(AtomicUsize::new(0));
    let disp = Arc::new(AtomicUsize::new(0));
    let inst = Arc::new(AtomicUsize::new(0));

    let mut host = PluginHost::new();
    host.add(Box::new(CountingPlugin {
        name: "only",
        register_count: reg.clone(),
        dispose_count: disp.clone(),
        instance_count: inst.clone(),
    }));

    let instances = host.create_instances().unwrap();
    let mut hooks = HooksRegistry::new();
    for instance in &instances {
        instance.register(&mut hooks);
    }
    assert_eq!(reg.load(Ordering::SeqCst), 1, "register 应被调用 1 次");
}

/// 重名插件 create_instances 失败
#[test]
fn create_instances_rejects_duplicate_names() {
    let mut host = PluginHost::new();
    host.add(Box::new(CountingPlugin {
        name: "dup",
        register_count: Arc::new(AtomicUsize::new(0)),
        dispose_count: Arc::new(AtomicUsize::new(0)),
        instance_count: Arc::new(AtomicUsize::new(0)),
    }));
    host.add(Box::new(CountingPlugin {
        name: "dup",
        register_count: Arc::new(AtomicUsize::new(0)),
        dispose_count: Arc::new(AtomicUsize::new(0)),
        instance_count: Arc::new(AtomicUsize::new(0)),
    }));

    let result = host.create_instances();
    assert!(matches!(
        result,
        Err(PluginInstallError::DuplicateName { ref name }) if name == "dup"
    ));
}

/// create_instance panic 的插件被跳过，其他插件仍生成实例
#[test]
fn create_instances_skips_plugin_that_panics() {
    struct PanicPlugin;
    impl Plugin for PanicPlugin {
        fn name(&self) -> &str {
            "panic_plugin"
        }
        fn create_instance(&self) -> Box<dyn PluginInstance> {
            panic!("create_instance 故意崩溃");
        }
    }

    let mut host = PluginHost::new();
    host.add(Box::new(PanicPlugin));
    host.add(Box::new(CountingPlugin {
        name: "normal",
        register_count: Arc::new(AtomicUsize::new(0)),
        dispose_count: Arc::new(AtomicUsize::new(0)),
        instance_count: Arc::new(AtomicUsize::new(0)),
    }));

    let instances = host.create_instances().unwrap();
    assert_eq!(instances.len(), 1, "panic 插件应被跳过，只保留 normal");
}

// ---------------------------------------------------------------------------
// PluginHost：dispose_all
// ---------------------------------------------------------------------------

/// dispose_all 逆序（LIFO）调用所有插件的 dispose
#[test]
fn dispose_all_calls_in_reverse_order() {
    let order = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));

    struct OrderedPlugin {
        name: &'static str,
        order: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }
    impl Plugin for OrderedPlugin {
        fn name(&self) -> &str {
            self.name
        }
        fn create_instance(&self) -> Box<dyn PluginInstance> {
            Box::new(CountingInstance {
                register_count: Arc::new(AtomicUsize::new(0)),
                dispose_count: Arc::new(AtomicUsize::new(0)),
            })
        }
        fn dispose(&self) {
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

    host.dispose_all();

    let order = order.lock().unwrap();
    assert_eq!(*order, vec!["third", "second", "first"]);
}

/// dispose_all 的 panic 防护：单个插件 dispose 崩溃不阻塞其他
#[test]
fn dispose_all_panic_protection() {
    struct PanicDispose;
    impl Plugin for PanicDispose {
        fn name(&self) -> &str {
            "panic_dispose"
        }
        fn create_instance(&self) -> Box<dyn PluginInstance> {
            Box::new(CountingInstance {
                register_count: Arc::new(AtomicUsize::new(0)),
                dispose_count: Arc::new(AtomicUsize::new(0)),
            })
        }
        fn dispose(&self) {
            panic!("dispose 故意崩溃");
        }
    }

    let called = Arc::new(AtomicUsize::new(0));
    struct NormalDispose {
        called: Arc<AtomicUsize>,
    }
    impl Plugin for NormalDispose {
        fn name(&self) -> &str {
            "normal_dispose"
        }
        fn create_instance(&self) -> Box<dyn PluginInstance> {
            Box::new(CountingInstance {
                register_count: Arc::new(AtomicUsize::new(0)),
                dispose_count: Arc::new(AtomicUsize::new(0)),
            })
        }
        fn dispose(&self) {
            self.called.fetch_add(1, Ordering::SeqCst);
        }
    }

    let mut host = PluginHost::new();
    host.add(Box::new(NormalDispose {
        called: called.clone(),
    }));
    host.add(Box::new(PanicDispose));

    host.dispose_all();

    // 逆序：PanicDispose 先 dispose（panic 被捕获），NormalDispose 后 dispose（正常）
    assert_eq!(called.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// PluginHost：list / validate_unique_names
// ---------------------------------------------------------------------------

/// list 返回所有插件名（注册顺序）
#[test]
fn list_returns_plugin_names_in_order() {
    let mut host = PluginHost::new();
    host.add(Box::new(CountingPlugin {
        name: "a",
        register_count: Arc::new(AtomicUsize::new(0)),
        dispose_count: Arc::new(AtomicUsize::new(0)),
        instance_count: Arc::new(AtomicUsize::new(0)),
    }));
    host.add(Box::new(CountingPlugin {
        name: "b",
        register_count: Arc::new(AtomicUsize::new(0)),
        dispose_count: Arc::new(AtomicUsize::new(0)),
        instance_count: Arc::new(AtomicUsize::new(0)),
    }));

    assert_eq!(host.list(), vec!["a", "b"]);
}

/// validate_unique_names 检测到重名返回 Err
#[test]
fn validate_unique_names_detects_duplicates() {
    let mut host = PluginHost::new();
    host.add(Box::new(CountingPlugin {
        name: "same",
        register_count: Arc::new(AtomicUsize::new(0)),
        dispose_count: Arc::new(AtomicUsize::new(0)),
        instance_count: Arc::new(AtomicUsize::new(0)),
    }));
    host.add(Box::new(CountingPlugin {
        name: "same",
        register_count: Arc::new(AtomicUsize::new(0)),
        dispose_count: Arc::new(AtomicUsize::new(0)),
        instance_count: Arc::new(AtomicUsize::new(0)),
    }));

    let result = host.validate_unique_names();
    assert!(matches!(
        result,
        Err(PluginInstallError::DuplicateName { ref name }) if name == "same"
    ));
}

/// validate_unique_names 无重名返回 Ok
#[test]
fn validate_unique_names_ok_when_unique() {
    let mut host = PluginHost::new();
    host.add(Box::new(CountingPlugin {
        name: "a",
        register_count: Arc::new(AtomicUsize::new(0)),
        dispose_count: Arc::new(AtomicUsize::new(0)),
        instance_count: Arc::new(AtomicUsize::new(0)),
    }));
    host.add(Box::new(CountingPlugin {
        name: "b",
        register_count: Arc::new(AtomicUsize::new(0)),
        dispose_count: Arc::new(AtomicUsize::new(0)),
        instance_count: Arc::new(AtomicUsize::new(0)),
    }));

    assert!(host.validate_unique_names().is_ok());
}

// ---------------------------------------------------------------------------
// Plugin trait：identity 默认实现
// ---------------------------------------------------------------------------

/// Plugin::identity() 默认从 name() 桥接
#[test]
fn plugin_identity_defaults_from_name() {
    let plugin = CountingPlugin {
        name: "my_plugin",
        register_count: Arc::new(AtomicUsize::new(0)),
        dispose_count: Arc::new(AtomicUsize::new(0)),
        instance_count: Arc::new(AtomicUsize::new(0)),
    };
    assert_eq!(plugin.identity().name, "my_plugin");
}

// ---------------------------------------------------------------------------
// PluginInstance trait：默认 dispose 不 panic
// ---------------------------------------------------------------------------

/// PluginInstance 默认 dispose 实现为空，不 panic
#[test]
fn plugin_instance_default_dispose_noop() {
    struct NoopInstance;
    impl PluginInstance for NoopInstance {
        fn register(&self, _hooks: &mut HooksRegistry) {}
    }
    let instance = NoopInstance;
    instance.dispose(); // 默认实现，不应 panic
}

// ---------------------------------------------------------------------------
// SessionSender：三通道分流 + 身份绑定
// ---------------------------------------------------------------------------

/// 构造测试用 SessionSender + 三条接收端
fn make_sender() -> (
    SessionSender,
    tokio::sync::mpsc::Receiver<OutputUserMessage>,
    tokio::sync::mpsc::Receiver<InterruptMessage>,
    tokio::sync::mpsc::Receiver<PluginMessage>,
) {
    let (tx_user, rx_user) = tokio::sync::mpsc::channel(16);
    let (tx_interrupt, rx_interrupt) = tokio::sync::mpsc::channel(16);
    let (tx_plugin, rx_plugin) = tokio::sync::mpsc::channel(16);
    let sender = SessionSender::new(
        PluginEventSource {
            name: "test_plugin".into(),
        },
        tx_user,
        tx_interrupt,
        tx_plugin,
    );
    (sender, rx_user, rx_interrupt, rx_plugin)
}

/// send_user 默认 Guide 模式
#[tokio::test]
async fn sender_send_user_uses_guide_mode_by_default() {
    let (sender, mut rx_user, _rx_int, _rx_plug) = make_sender();
    sender.send_user("hello");
    let received = rx_user.recv().await.expect("应收到 User 消息");
    assert_eq!(received.payload.content, "hello");
    assert_eq!(received.payload.mode, UserMessageMode::Guide);
}

/// send_user_with_mode 指定 Pending 模式
#[tokio::test]
async fn sender_send_user_with_mode_pending() {
    let (sender, mut rx_user, _rx_int, _rx_plug) = make_sender();
    sender.send_user_with_mode("排队", UserMessageMode::Pending);
    let received = rx_user.recv().await.expect("应收到 User 消息");
    assert_eq!(received.payload.content, "排队");
    assert_eq!(received.payload.mode, UserMessageMode::Pending);
}

/// send_interrupt 投递到 Interrupt 通道，source = Hook
#[tokio::test]
async fn sender_send_interrupt_routes_to_interrupt_channel() {
    let (sender, _rx_user, mut rx_int, _rx_plug) = make_sender();
    sender.send_interrupt("循环检测");
    let received = rx_int.recv().await.expect("应收到 Interrupt 消息");
    assert_eq!(received.payload.reason, "循环检测");
    assert_eq!(received.payload.source, InterruptSource::Hook);
}

/// send_plugin 自动填 source = identity
#[tokio::test]
async fn sender_send_plugin_fills_source_from_identity() {
    let (sender, _rx_user, _rx_int, mut rx_plug) = make_sender();
    sender.send_plugin("warn", "检测到异常");
    let received = rx_plug.recv().await.expect("应收到 Plugin 消息");
    assert_eq!(received.payload.source.name, "test_plugin");
    assert_eq!(received.payload.event_type, "warn");
    assert_eq!(received.payload.message.as_deref(), Some("检测到异常"));
    assert!(received.payload.data.is_none());
    assert!(received.payload.error.is_none());
}

/// send_plugin_data 带 data 字段
#[tokio::test]
async fn sender_send_plugin_data_carries_payload() {
    let (sender, _rx_user, _rx_int, mut rx_plug) = make_sender();
    sender.send_plugin_data("cumulative", serde_json::json!({"count": 42}));
    let received = rx_plug.recv().await.expect("应收到 Plugin 消息");
    assert_eq!(received.payload.source.name, "test_plugin");
    assert_eq!(received.payload.event_type, "cumulative");
    assert_eq!(received.payload.data.unwrap()["count"], 42);
    assert!(received.payload.message.is_none());
}

/// send_plugin_full 同时带 data + message
#[tokio::test]
async fn sender_send_plugin_full_with_all_fields() {
    let (sender, _rx_user, _rx_int, mut rx_plug) = make_sender();
    sender.send_plugin_full(
        "report",
        Some(serde_json::json!({"key": "value"})),
        Some("err".to_string()),
        Some("msg".to_string()),
    );
    let received = rx_plug.recv().await.expect("应收到 Plugin 消息");
    assert_eq!(received.payload.event_type, "report");
    assert_eq!(received.payload.data.unwrap()["key"], "value");
    assert_eq!(received.payload.error.as_deref(), Some("err"));
    assert_eq!(received.payload.message.as_deref(), Some("msg"));
}

/// 三通道完全隔离：发 User 不影响 Interrupt/Plugin 通道
#[tokio::test]
async fn sender_three_channels_are_independent() {
    let (sender, mut rx_user, mut rx_int, mut rx_plug) = make_sender();
    sender.send_user("只发 User");
    // 其他两个通道应无消息
    assert!(rx_int.try_recv().is_err(), "Interrupt 通道不应有消息");
    assert!(rx_plug.try_recv().is_err(), "Plugin 通道不应有消息");
    // User 通道有消息
    let received = rx_user.recv().await.expect("User 通道应有消息");
    assert_eq!(received.payload.content, "只发 User");
}

/// identity() 只读访问
#[test]
fn sender_identity_is_readable() {
    let (sender, _rx_user, _rx_int, _rx_plug) = make_sender();
    assert_eq!(sender.identity().name, "test_plugin");
}

/// SessionSender 是 Clone（每插件实例持有自己的克隆）
#[test]
fn sender_is_cloneable() {
    let (sender, _rx_user, _rx_int, _rx_plug) = make_sender();
    let cloned = sender.clone();
    assert_eq!(cloned.identity().name, "test_plugin");
}
