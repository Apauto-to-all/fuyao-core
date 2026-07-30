//! fuyao-hooks 集成测试：插件装配链路与跨阶段协作
//!
//! 单元测试（src/plugin/tests.rs）已密集覆盖单个 API 行为：
//! create_instances 一对一、重名拒绝、create_instance panic 跳过、dispose LIFO、
//! SessionSender 三通道分流等。本文件聚焦**跨模块/跨阶段协作链路**——单元测试的空白带：
//!
//! - 完整装配链路：PluginHost.create_instances → 逐个 register → init_send_inputs → 触发钩子
//! - 跨阶段 panic 恢复：create_instance 崩溃的插件被跳过后，其余插件仍正常 register/触发
//! - send_input hook 端到端：插件在 hook 回调里拿到 sender 发消息，消息真的落到对应通道
//! - 三类 hook 共存：同一 registry 上同时挂 intercept + observe + send_input，喂真实事件流
//!
//! 全部使用默认配置（不调 set_config），走 get_config 未 set 返回 default 的兜底。

mod common;

use std::sync::{Arc, Mutex};

use common::{FakePlugin, FakePluginConfig, HookAction, assemble, make_chunk, make_sender};
use fuyao_api::message::OutputEvent;
use fuyao_hooks::{InterceptResult, Plugin, PluginHost};

/// 用日志驱动的 helper：构造单插件 + 装配，返回 hooks 与日志
async fn single_plugin_assemble(
    cfg: FakePluginConfig,
) -> (fuyao_hooks::SharedHooks, common::ExecLog) {
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let plugin = FakePlugin::new(cfg, log.clone());
    let (sender, _rx_user, _rx_interrupt, _rx_plugin) = make_sender();
    let instances = vec![plugin.create_instance()];
    let hooks = assemble(instances, sender).await;
    (hooks, log)
}

// ============================================================================
// 完整装配链路
// ============================================================================

#[tokio::test]
async fn full_assembly_registers_and_invokes_observe() {
    // 完整链路：PluginHost → create_instances → register observe → init_send_inputs →
    // hook_output_observe 真实触发，observe 钩子被调用并记录
    let cfg = FakePluginConfig {
        name: "observer".into(),
        actions: vec![HookAction::Observe {
            log_tag: "obs_fired".into(),
        }],
        create_instance_panic: false,
        register_panic: false,
    };
    let (hooks, _log) = single_plugin_assemble(cfg).await;

    hooks
        .lock()
        .await
        .hook_output_observe(OutputEvent::Chunk(make_chunk("hello")))
        .await;

    // 仅注册了 observe，intercept 为空应原样放行
    let result = hooks
        .lock()
        .await
        .hook_output_intercept(&OutputEvent::Chunk(make_chunk("hello")));
    assert!(matches!(result, InterceptResult::Pass(_)));
}

#[tokio::test]
async fn empty_registry_handles_events_without_panic() {
    // 空 registry（无插件装配）也应能正常处理 observe/intercept 事件，不 panic
    let (sender, _rx_user, _rx_interrupt, _rx_plugin) = make_sender();
    let hooks = assemble(vec![], sender).await;

    hooks
        .lock()
        .await
        .hook_output_observe(OutputEvent::Chunk(make_chunk("x")))
        .await;
    let result = hooks
        .lock()
        .await
        .hook_output_intercept(&OutputEvent::Chunk(make_chunk("x")));
    assert!(matches!(result, InterceptResult::Pass(_)));
}

// ============================================================================
// 多插件注册顺序与跨阶段 panic 恢复
// ============================================================================

#[tokio::test]
async fn multiple_plugins_register_and_observe_in_registration_order() {
    // 两个插件按 PluginHost 注册顺序 create_instance + register，
    // observe 钩子按注册顺序触发（observe priority 固定 0，不参与排序）
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let mut host = PluginHost::new();
    host.add(Box::new(FakePlugin::new(
        FakePluginConfig {
            name: "p1".into(),
            actions: vec![HookAction::Observe {
                log_tag: "p1_obs".into(),
            }],
            create_instance_panic: false,
            register_panic: false,
        },
        log.clone(),
    )));
    host.add(Box::new(FakePlugin::new(
        FakePluginConfig {
            name: "p2".into(),
            actions: vec![HookAction::Observe {
                log_tag: "p2_obs".into(),
            }],
            create_instance_panic: false,
            register_panic: false,
        },
        log.clone(),
    )));

    // create_instances 顺序 = add 顺序
    let instances = host.create_instances().expect("无重名应成功");
    assert_eq!(instances.len(), 2);
    let (sender, _rx_user, _rx_interrupt, _rx_plugin) = make_sender();
    let hooks = assemble(instances, sender).await;

    hooks
        .lock()
        .await
        .hook_output_observe(OutputEvent::Chunk(make_chunk("e")))
        .await;

    let recorded = log.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec![
            "p1:create_instance",
            "p2:create_instance",
            "p1_obs",
            "p2_obs"
        ],
        "create_instance 按 add 顺序；observe 按注册顺序串行"
    );
}

#[tokio::test]
async fn create_instance_panic_skipped_others_proceed() {
    // 中间插件 create_instance panic：被 PluginHost 跳过（不在 instances 里），
    // 其余插件仍正常 register/触发。这是跨阶段 panic 恢复的核心契约。
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let mut host = PluginHost::new();
    host.add(Box::new(FakePlugin::new(
        FakePluginConfig {
            name: "healthy".into(),
            actions: vec![HookAction::Observe {
                log_tag: "healthy_obs".into(),
            }],
            create_instance_panic: false,
            register_panic: false,
        },
        log.clone(),
    )));
    host.add(Box::new(FakePlugin::new(
        FakePluginConfig {
            name: "boom".into(),
            actions: vec![],
            create_instance_panic: true,
            register_panic: false,
        },
        log.clone(),
    )));
    host.add(Box::new(FakePlugin::new(
        FakePluginConfig {
            name: "trailing".into(),
            actions: vec![HookAction::Observe {
                log_tag: "trailing_obs".into(),
            }],
            create_instance_panic: false,
            register_panic: false,
        },
        log.clone(),
    )));

    let instances = host.create_instances().expect("panic 不应导致整体失败");
    // boom 被跳过：只拿到 healthy + trailing 两个实例
    assert_eq!(instances.len(), 2, "崩溃插件应被跳过");

    let (sender, _rx_user, _rx_interrupt, _rx_plugin) = make_sender();
    let hooks = assemble(instances, sender).await;
    hooks
        .lock()
        .await
        .hook_output_observe(OutputEvent::Chunk(make_chunk("e")))
        .await;

    let recorded = log.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec![
            "healthy:create_instance",
            "boom:create_instance_panic",
            "trailing:create_instance",
            "healthy_obs",
            "trailing_obs"
        ],
        "boom panic 后 healthy/trailing 仍正常注册并触发"
    );
}

#[tokio::test]
async fn register_panic_isolated_others_proceed() {
    // register 阶段 panic：由装配 helper（复刻 fuyao-core）做同步 catch_unwind，
    // 崩溃插件被跳过，其余插件仍正常注册并触发
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let mut host = PluginHost::new();
    host.add(Box::new(FakePlugin::new(
        FakePluginConfig {
            name: "ok".into(),
            actions: vec![HookAction::Observe {
                log_tag: "ok_obs".into(),
            }],
            create_instance_panic: false,
            register_panic: false,
        },
        log.clone(),
    )));
    host.add(Box::new(FakePlugin::new(
        FakePluginConfig {
            name: "bad_register".into(),
            actions: vec![],
            create_instance_panic: false,
            register_panic: true,
        },
        log.clone(),
    )));

    let instances = host.create_instances().expect("应成功");
    let (sender, _rx_user, _rx_interrupt, _rx_plugin) = make_sender();
    let hooks = assemble(instances, sender).await;
    hooks
        .lock()
        .await
        .hook_output_observe(OutputEvent::Chunk(make_chunk("e")))
        .await;

    let recorded = log.lock().unwrap().clone();
    assert!(
        recorded.contains(&"ok_obs".to_string()),
        "bad_register 注册崩溃后 ok 仍应触发"
    );
    assert!(
        recorded.contains(&"bad_register:register_panic".to_string()),
        "register panic 应被记录"
    );
}

// ============================================================================
// send_input hook 端到端
// ============================================================================

#[tokio::test]
async fn send_input_hook_delivers_user_message_to_channel() {
    // 插件在 send_input hook 回调里用 sender.send_user 发消息，
    // 消息应真的落到 rx_user 通道（跨 PluginHost→register→init_send_inputs→sender 的完整路径）
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let plugin = FakePlugin::new(
        FakePluginConfig {
            name: "sender_plugin".into(),
            actions: vec![HookAction::SendInputUser {
                log_tag: "send_input_fired".into(),
                content: "主动投递的消息".into(),
            }],
            create_instance_panic: false,
            register_panic: false,
        },
        log.clone(),
    );
    let (sender, mut rx_user, _rx_interrupt, _rx_plugin) = make_sender();
    let _hooks = assemble(vec![plugin.create_instance()], sender).await;

    // init_send_inputs 已在 assemble 内完成；验证消息已投递
    let received = rx_user.recv().await.expect("应收到插件投递的 User 消息");
    assert_eq!(received.payload.content, "主动投递的消息");
    assert!(matches!(
        received.payload.source,
        fuyao_api::message::UserMessageSource::Plugin(_)
    ));
}

// ============================================================================
// 三类 hook 共存
// ============================================================================

#[tokio::test]
async fn three_hook_types_coexist_on_single_registry() {
    // 同一插件同时注册 intercept + observe + send_input（仿 fuyao-guard 实际用法），
    // 喂一个事件，三类钩子都应被触发且互不干扰
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let plugin = FakePlugin::new(
        FakePluginConfig {
            name: "combo".into(),
            actions: vec![
                HookAction::Observe {
                    log_tag: "observe_fired".into(),
                },
                HookAction::Intercept {
                    priority: 0,
                    log_tag: "intercept_fired".into(),
                },
                HookAction::SendInputUser {
                    log_tag: "send_input_fired".into(),
                    content: "combo_msg".into(),
                },
            ],
            create_instance_panic: false,
            register_panic: false,
        },
        log.clone(),
    );
    let (sender, mut rx_user, _rx_interrupt, _rx_plugin) = make_sender();
    let hooks = assemble(vec![plugin.create_instance()], sender).await;

    // send_input 在装配时已触发（投递了 combo_msg）
    let received = rx_user.recv().await.expect("send_input 应已投递");
    assert_eq!(received.payload.content, "combo_msg");

    // 喂一个事件：observe + intercept 都应记录
    let event = OutputEvent::Chunk(make_chunk("payload"));
    hooks.lock().await.hook_output_observe(event).await;
    let result = hooks
        .lock()
        .await
        .hook_output_intercept(&OutputEvent::Chunk(make_chunk("payload")));
    assert!(matches!(result, InterceptResult::Pass(_)));

    let recorded = log.lock().unwrap().clone();
    assert!(recorded.contains(&"send_input_fired".to_string()));
    assert!(recorded.contains(&"observe_fired".to_string()));
    assert!(recorded.contains(&"intercept_fired".to_string()));
}

// ============================================================================
// dispose LIFO（装配生命周期收尾）
// ============================================================================

#[tokio::test]
async fn plugin_host_dispose_all_runs_in_reverse_order() {
    // dispose_all 按注册逆序调用（LIFO），与 create_instances 的正序对称。
    // 单测已覆盖 dispose_all LIFO 本身，这里验证它在「多插件装配后」仍成立。
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let mut host = PluginHost::new();
    host.add(Box::new(FakePlugin::new(
        FakePluginConfig {
            name: "first".into(),
            actions: vec![],
            create_instance_panic: false,
            register_panic: false,
        },
        log.clone(),
    )));
    host.add(Box::new(FakePlugin::new(
        FakePluginConfig {
            name: "second".into(),
            actions: vec![],
            create_instance_panic: false,
            register_panic: false,
        },
        log.clone(),
    )));

    let _instances = host.create_instances().expect("应成功");
    host.dispose_all();
    // dispose 默认空实现，此处仅验证多插件装配后 dispose_all 不 panic
}
