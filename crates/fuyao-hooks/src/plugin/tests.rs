//! plugin 模块单元测试
//!
//! 覆盖：
//! - PluginHost（create_instances / dispose_all / validate_unique_names / list / panic 防护）
//! - PluginInstance trait register 行为（含 sender 接收）
//! - SessionSender 三通道分流 + 身份绑定
//! - simple_plugin 快捷构造（钩子真实生效 / name / 每 session 独立调用闭包）

use super::factory::Plugin;
use super::host::{PluginHost, PluginInstallError};
use super::instance::PluginInstance;
use super::sender::SessionSender;
use super::simple::simple_plugin;
use crate::HooksRegistry;
use crate::test_util::{empty_chunk_event, make_sender};
use fuyao_api::InterruptSource;
use fuyao_api::UserMessageMode;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::QueueEntry;
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
    fn register(&self, _hooks: &mut HooksRegistry, _sender: &SessionSender) {
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

/// create_instances 为每个插件生成一个 (名, 实例) 配对
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
    let names: Vec<&str> = instances.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["a", "b"], "配对的插件名应与注册顺序一致");
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
    let (tx_inbound, _rx_inbound) = tokio::sync::mpsc::channel(16);
    let (tx_interrupt, _rx_interrupt) = tokio::sync::mpsc::channel(16);
    let (tx_event, _rx_event) = tokio::sync::mpsc::unbounded_channel::<OutputEvent>();
    let sender = SessionSender::new("only", "test-session", tx_inbound, tx_interrupt, tx_event);
    for (_, instance) in &instances {
        instance.register(&mut hooks, &sender);
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
// PluginInstance trait：默认 dispose 不 panic
// ---------------------------------------------------------------------------

/// PluginInstance 默认 dispose 实现为空，不 panic
#[test]
fn plugin_instance_default_dispose_noop() {
    struct NoopInstance;
    impl PluginInstance for NoopInstance {
        fn register(&self, _hooks: &mut HooksRegistry, _sender: &SessionSender) {}
    }
    let instance = NoopInstance;
    instance.dispose(); // 默认实现，不应 panic
}

// ---------------------------------------------------------------------------
// SessionSender：两通道分流 + 身份绑定
// ---------------------------------------------------------------------------

/// send_user 指定 Guide 模式
#[tokio::test]
async fn sender_send_user_guide_mode() {
    let (sender, mut rx_inbound, _rx_int, _rx_event) = make_sender("test_plugin", "test-session");
    sender.send_user("hello", UserMessageMode::Guide);
    let received = rx_inbound.recv().await.expect("应收到入站条目");
    let QueueEntry::User(m) = received else {
        panic!("应为 User 条目");
    };
    assert_eq!(m.payload.content, "hello");
    assert_eq!(m.payload.mode, UserMessageMode::Guide);
}

/// send_user 指定 Pending 模式
#[tokio::test]
async fn sender_send_user_pending_mode() {
    let (sender, mut rx_inbound, _rx_int, _rx_event) = make_sender("test_plugin", "test-session");
    sender.send_user("排队", UserMessageMode::Pending);
    let received = rx_inbound.recv().await.expect("应收到入站条目");
    let QueueEntry::User(m) = received else {
        panic!("应为 User 条目");
    };
    assert_eq!(m.payload.content, "排队");
    assert_eq!(m.payload.mode, UserMessageMode::Pending);
}

/// send_user 自动填 source = Plugin（绑插件名）
#[tokio::test]
async fn sender_send_user_fills_plugin_source() {
    let (sender, mut rx_inbound, _rx_int, _rx_event) = make_sender("test_plugin", "test-session");
    sender.send_user("来源校验", UserMessageMode::Guide);
    let received = rx_inbound.recv().await.expect("应收到入站条目");
    let QueueEntry::User(m) = received else {
        panic!("应为 User 条目");
    };
    match m.payload.source {
        fuyao_api::UserMessageSource::Plugin(src) => assert_eq!(src.name, "test_plugin"),
        other => panic!("source 应为 Plugin，实际 {other:?}"),
    }
}

/// send_interrupt 投递到 Interrupt 通道，source = Hook
#[tokio::test]
async fn sender_send_interrupt_routes_to_interrupt_channel() {
    let (sender, _rx_inbound, mut rx_int, _rx_event) = make_sender("test_plugin", "test-session");
    sender.send_interrupt("循环检测");
    let received = rx_int.recv().await.expect("应收到 Interrupt 消息");
    assert_eq!(received.payload.reason, "循环检测");
    assert_eq!(received.payload.source, InterruptSource::Hook);
}

/// 入站与中断通道完全隔离：发 User 不影响 Interrupt 通道
#[tokio::test]
async fn sender_two_channels_are_independent() {
    let (sender, mut rx_inbound, mut rx_int, _rx_event) =
        make_sender("test_plugin", "test-session");
    sender.send_user("只发 User", UserMessageMode::Guide);
    // Interrupt 通道应无消息
    assert!(rx_int.try_recv().is_err(), "Interrupt 通道不应有消息");
    // 入站通道有消息
    let received = rx_inbound.recv().await.expect("入站通道应有消息");
    let QueueEntry::User(m) = received else {
        panic!("应为 User 条目");
    };
    assert_eq!(m.payload.content, "只发 User");
}

/// name() 只读访问
#[test]
fn sender_name_is_readable() {
    let (sender, _rx_inbound, _rx_int, _rx_event) = make_sender("test_plugin", "test-session");
    assert_eq!(sender.name(), "test_plugin");
}

/// SessionSender 是 Clone（每插件实例持有自己的克隆）
#[test]
fn sender_is_cloneable() {
    let (sender, _rx_inbound, _rx_int, _rx_event) = make_sender("test_plugin", "test-session");
    let cloned = sender.clone();
    assert_eq!(cloned.name(), "test_plugin");
}

// ---------------------------------------------------------------------------
// simple_plugin：无状态插件快捷构造
// ---------------------------------------------------------------------------

/// simple_plugin：name 正确，注册的观察钩子真实生效
#[tokio::test]
async fn simple_plugin_registers_effective_observe_hook() {
    let observed = Arc::new(AtomicUsize::new(0));

    let plugin = {
        let observed = observed.clone();
        simple_plugin("simple_observer", move |hooks, _sender| {
            let observed = observed.clone();
            hooks.register_output_observe(
                0,
                Arc::new(move |_msg| {
                    let observed = observed.clone();
                    Box::pin(async move {
                        observed.fetch_add(1, Ordering::SeqCst);
                    })
                }),
            );
        })
    };

    assert_eq!(plugin.name(), "simple_observer");

    let instance = plugin.create_instance();
    let mut hooks = HooksRegistry::new();
    instance.register(
        &mut hooks,
        &make_sender("simple_observer", "test-session").0,
    );
    hooks.finalize();

    hooks
        .hook_output_observe(Arc::new(empty_chunk_event()))
        .await;
    assert_eq!(
        observed.load(Ordering::SeqCst),
        1,
        "观察钩子应被真实注册并执行"
    );
}

/// simple_plugin：注册的拦截钩子真实生效（原地修改事件）
#[test]
fn simple_plugin_registers_effective_intercept_hook() {
    let plugin = simple_plugin("simple_intercept", |hooks, _sender| {
        hooks.register_output_intercept(
            0,
            Arc::new(|ev| {
                if let OutputEvent::Chunk(msg) = ev {
                    msg.payload.content = Some("simple 拦截改写".into());
                }
                None
            }),
        );
    });

    let instance = plugin.create_instance();
    let mut hooks = HooksRegistry::new();
    instance.register(
        &mut hooks,
        &make_sender("simple_intercept", "test-session").0,
    );
    hooks.finalize();

    let mut ev = empty_chunk_event();
    assert!(hooks.hook_output_intercept(&mut ev).is_none());
    let content = match ev {
        OutputEvent::Chunk(msg) => msg.payload.content,
        _ => None,
    };
    assert_eq!(content.as_deref(), Some("simple 拦截改写"));
}

/// create_instance 每 session 独立调用闭包：两个 session 各装配一次、各生效一份钩子
#[tokio::test]
async fn simple_plugin_create_instance_per_session() {
    // 高 2 位记注册调用次数（每 session 1 次），低 2 位记钩子执行次数（每 session 10 次）
    let counter = Arc::new(AtomicUsize::new(0));

    let plugin = {
        let counter = counter.clone();
        simple_plugin("multi_session", move |hooks, _sender| {
            counter.fetch_add(1, Ordering::SeqCst);
            let counter = counter.clone();
            hooks.register_output_observe(
                0,
                Arc::new(move |_msg| {
                    let counter = counter.clone();
                    Box::pin(async move {
                        counter.fetch_add(10, Ordering::SeqCst);
                    })
                }),
            );
        })
    };

    // 模拟两个 session 各自装配
    for _ in 0..2 {
        let instance = plugin.create_instance();
        let mut hooks = HooksRegistry::new();
        instance.register(&mut hooks, &make_sender("multi_session", "test-session").0);
        hooks.finalize();
        hooks
            .hook_output_observe(Arc::new(empty_chunk_event()))
            .await;
    }

    // 2 次注册调用（各 +1）+ 2 次钩子执行（各 +10）
    assert_eq!(counter.load(Ordering::SeqCst), 22);
}
