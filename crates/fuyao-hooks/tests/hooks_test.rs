//! fuyao-hooks 集成测试
//!
//! 钉死 Plugin ↔ PluginHost ↔ HooksRegistry 的装配闭环契约：
//! - 完整装配链路（plugin.register 注册的钩子能被 hook_* 真正触发）
//! - 多 plugin 装配与钩子执行顺序
//! - panic 防护贯穿（单个插件崩溃不阻塞其他插件）
//! - dispose 逆序（LIFO）
//! - 重名硬失败
//! - 拦截短路、before_llm OR 语义
//!
//! 超时行为依赖私有字段 hook_timeout，集成测试不可注入，由单元测试覆盖。

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use fuyao_api::Message;
use fuyao_api::message::EventBase;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::output::{ChunkMessage, ChunkPayload};
use fuyao_hooks::{HooksRegistry, InterceptResult, PluginHost, PluginInstallError, SharedHooks};

// ---------------------------------------------------------------------------
// PluginHost 装配基础
// ---------------------------------------------------------------------------

#[tokio::test]
async fn install_invokes_register_on_all_plugins() {
    let plugin_a = common::CountingPlugin::new("a");
    let plugin_b = common::CountingPlugin::new("b");
    let reg_a = plugin_a.register_count.clone();
    let reg_b = plugin_b.register_count.clone();

    let mut host = PluginHost::new();
    host.add(Box::new(plugin_a));
    host.add(Box::new(plugin_b));

    let hooks = common::empty_hooks();
    host.install(&hooks).await.unwrap();

    assert_eq!(
        reg_a.load(Ordering::SeqCst),
        1,
        "plugin a 的 register 应被调用一次"
    );
    assert_eq!(
        reg_b.load(Ordering::SeqCst),
        1,
        "plugin b 的 register 应被调用一次"
    );
}

#[tokio::test]
async fn list_returns_plugin_names_in_add_order() {
    let mut host = PluginHost::new();
    host.add(Box::new(common::CountingPlugin::new("first")));
    host.add(Box::new(common::CountingPlugin::new("second")));
    assert_eq!(host.list(), vec!["first", "second"]);
}

// ---------------------------------------------------------------------------
// 重名硬失败
// ---------------------------------------------------------------------------

#[tokio::test]
async fn install_rejects_duplicate_plugin_names() {
    let mut host = PluginHost::new();
    host.add(Box::new(common::CountingPlugin::new("dup")));
    host.add(Box::new(common::CountingPlugin::new("dup")));

    let hooks = common::empty_hooks();
    let result = host.install(&hooks).await;
    assert!(
        matches!(result, Err(PluginInstallError::DuplicateName { ref name }) if name == "dup"),
        "重名应硬失败并返回 DuplicateName"
    );
}

// ---------------------------------------------------------------------------
// 完整装配链路：plugin.register 注册的钩子能被 hook_* 真正触发
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plugin_registered_observe_hook_fires_on_hook_output_observe() {
    // 这是单元测试的空白带：plugin → install → SharedHooks → hook_output_observe 完整链路
    let observe_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let plugin = common::ObservePlugin::new("observer", observe_count.clone());

    let mut host = PluginHost::new();
    host.add(Box::new(plugin));
    let hooks = common::empty_hooks();
    host.install(&hooks).await.unwrap();

    // 触发 observe
    let msg = sample_chunk_event();
    hooks.lock().await.hook_output_observe(msg).await;
    assert_eq!(
        observe_count.load(Ordering::SeqCst),
        1,
        "插件注册的 observe 钩子应被触发"
    );
}

#[tokio::test]
async fn plugin_registered_intercept_hook_blocks_output() {
    let plugin = common::InterceptPlugin::new("blocker", true);
    let mut host = PluginHost::new();
    host.add(Box::new(plugin));
    let hooks = common::empty_hooks();
    host.install(&hooks).await.unwrap();

    let result = hooks
        .lock()
        .await
        .hook_output_intercept(&sample_chunk_event());
    assert!(
        matches!(result, InterceptResult::Block(ref reason) if reason == "插件拦截"),
        "拦截型插件应使 hook_output_intercept 返回 Block"
    );
}

#[tokio::test]
async fn plugin_registered_before_llm_hook_injects_messages() {
    let injected = vec![Message::user("注入消息".to_string())];
    let plugin = common::BeforeLlmPlugin::new("injector", injected.clone(), false);
    let mut host = PluginHost::new();
    host.add(Box::new(plugin));
    let hooks = common::empty_hooks();
    host.install(&hooks).await.unwrap();

    let output = hooks.lock().await.hook_before_llm().await;
    assert_eq!(output.messages.len(), 1, "before_llm 应注入插件提供的消息");
    assert!(!output.skip_tools, "未请求 skip_tools");
}

#[tokio::test]
async fn before_llm_skip_tools_or_semantics_across_plugins() {
    // 两个 before_llm 插件，一个不 skip，一个 skip → OR 语义最终 skip
    let plugin_a = common::BeforeLlmPlugin::new("a", vec![], false);
    let plugin_b = common::BeforeLlmPlugin::new("b", vec![Message::user("msg".to_string())], true);
    let mut host = PluginHost::new();
    host.add(Box::new(plugin_a));
    host.add(Box::new(plugin_b));
    let hooks = common::empty_hooks();
    host.install(&hooks).await.unwrap();

    let output = hooks.lock().await.hook_before_llm().await;
    assert!(output.skip_tools, "任一插件 skip 则最终生效（OR）");
    assert_eq!(output.messages.len(), 1, "末非空 messages 生效");
}

// ---------------------------------------------------------------------------
// panic 防护贯穿：单个插件 register 崩溃不阻塞其他插件
// ---------------------------------------------------------------------------

#[tokio::test]
async fn register_panic_does_not_block_other_plugins() {
    // plugin A register 时 panic，plugin B 正常 → install 应成功，B 的钩子仍在 registry
    let observe_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let panic_plugin = common::PanicOnRegisterPlugin::new("panicker");
    let normal_plugin = common::ObservePlugin::new("normal", observe_count.clone());

    let mut host = PluginHost::new();
    host.add(Box::new(panic_plugin));
    host.add(Box::new(normal_plugin));

    let hooks = common::empty_hooks();
    // install 应成功（panic 被防护，不返回 Err）
    host.install(&hooks).await.unwrap();

    // normal 插件的 observe 钩子应仍在 registry 中并触发
    hooks
        .lock()
        .await
        .hook_output_observe(sample_chunk_event())
        .await;
    assert_eq!(
        observe_count.load(Ordering::SeqCst),
        1,
        "panic 插件不应影响正常插件的钩子注册与执行"
    );
}

// ---------------------------------------------------------------------------
// dispose 逆序（LIFO）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dispose_all_invokes_in_reverse_order() {
    let dispose_order: Arc<std::sync::Mutex<Vec<&'static str>>> =
        Arc::new(std::sync::Mutex::new(vec![]));

    let mut host = PluginHost::new();
    host.add(Box::new(common::OrderedPlugin::new(
        "first",
        dispose_order.clone(),
    )));
    host.add(Box::new(common::OrderedPlugin::new(
        "second",
        dispose_order.clone(),
    )));
    host.add(Box::new(common::OrderedPlugin::new(
        "third",
        dispose_order.clone(),
    )));

    let hooks = common::empty_hooks();
    host.install(&hooks).await.unwrap();
    host.dispose_all().await;

    let order = dispose_order.lock().unwrap();
    assert_eq!(
        *order,
        vec!["third", "second", "first"],
        "dispose 应按 LIFO（逆序）调用"
    );
}

#[tokio::test]
async fn dispose_all_counts_dispose_calls() {
    let plugin = common::CountingPlugin::new("disposable");
    let dispose_count = plugin.dispose_count.clone();
    let mut host = PluginHost::new();
    host.add(Box::new(plugin));

    let hooks = common::empty_hooks();
    host.install(&hooks).await.unwrap();
    host.dispose_all().await;

    assert_eq!(
        dispose_count.load(Ordering::SeqCst),
        1,
        "dispose 应被调用一次"
    );
}

// ---------------------------------------------------------------------------
// 多 plugin observe 执行顺序（按注册顺序）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multiple_observe_plugins_all_fire() {
    let count_a = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count_b = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut host = PluginHost::new();
    host.add(Box::new(common::ObservePlugin::new(
        "obs-a",
        count_a.clone(),
    )));
    host.add(Box::new(common::ObservePlugin::new(
        "obs-b",
        count_b.clone(),
    )));

    let hooks = common::empty_hooks();
    host.install(&hooks).await.unwrap();

    hooks
        .lock()
        .await
        .hook_output_observe(sample_chunk_event())
        .await;
    assert_eq!(count_a.load(Ordering::SeqCst), 1);
    assert_eq!(
        count_b.load(Ordering::SeqCst),
        1,
        "两个 observe 插件都应触发"
    );
}

// ---------------------------------------------------------------------------
// 无钩子时的默认行为
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hook_before_llm_returns_default_when_no_hooks() {
    let registry = HooksRegistry::new();
    let hooks: SharedHooks = Arc::new(tokio::sync::Mutex::new(registry));
    let output = hooks.lock().await.hook_before_llm().await;
    assert!(output.messages.is_empty());
    assert!(!output.skip_tools);
}

#[tokio::test]
async fn hook_output_intercept_passes_when_no_hooks() {
    let registry = HooksRegistry::new();
    let hooks: SharedHooks = Arc::new(tokio::sync::Mutex::new(registry));
    let msg = sample_chunk_event();
    let result = hooks.lock().await.hook_output_intercept(&msg);
    assert!(
        matches!(result, InterceptResult::Pass(_)),
        "无拦截钩子应 Pass"
    );
}

// ---------------------------------------------------------------------------
// 辅助：构造样例 OutputEvent
// ---------------------------------------------------------------------------

fn sample_chunk_event() -> OutputEvent {
    OutputEvent::Chunk(ChunkMessage {
        base: EventBase::default(),
        payload: ChunkPayload {
            content: Some("文本".into()),
            reasoning: None,
        },
    })
}
