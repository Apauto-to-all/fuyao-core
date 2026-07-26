//! `App::create_detached_child_session` 的独占消费语义验证
//!
//! 设计文档 04 Phase 4：detached session 的事件不进 fan_out，调用方独占消费 rx。
//! 与 `App::create_child_session`（spawn forwarder，rx 进 fan_out）形成对比。
//!
//! 两个测试聚焦点：
//! - **detached**：调用方 `child_rx.recv()` 收到 detached session 的所有事件；
//!   `App::recv` 短超时确认 detached 事件**不**进 fan_out
//! - **常规 child**（对比）：常规 child session 的事件经 forwarder 推到 fan_out，
//!   `App::recv` 收到；调用方不持 rx

mod common;

use std::time::Duration;

use common::{MockProvider, temp_agent_paths, text_events};
use fuyao_api::message::input::{UserMessage, UserPayload};
use fuyao_api::message::{EventBase, InputEvent, OutputEvent};
use fuyao_api::{EngineParams, ModelConfig, SessionParams};
use fuyao_app::{App, LogGuard, build_tool_registry};
use fuyao_core::{ChildSessionSource, Engine, PluginHost};
use tokio::time::timeout;

/// 构造 Guide 模式用户消息（模型配置由 session 的 SessionParams 决定，不随消息走）
fn guide_msg(content: &str) -> InputEvent {
    InputEvent::User(UserMessage {
        base: EventBase::default(),
        payload: UserPayload {
            content: content.to_string(),
            mode: Default::default(),
            source: Default::default(),
        },
    })
}

/// 测试用 SessionParams：`test/model` 形式的 model_id 与 MockProvider 注册表匹配
fn session_params() -> SessionParams {
    SessionParams {
        model_config: ModelConfig {
            model_id: Some("test/model".to_string()),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// 把任意 Provider 包成 ProviderRegistry（统一 provider_id="test"）
fn as_providers<P: fuyao_provider::Provider + 'static>(p: P) -> fuyao_provider::ProviderRegistry {
    fuyao_provider::ProviderRegistry::with_instance("test", std::sync::Arc::new(p))
}

/// 提取事件的 session_id（enum 各变体的 base 均自带 session_id）
fn session_id_of(ev: &OutputEvent) -> &str {
    match ev {
        OutputEvent::Chunk(m) => m.base.session_id.as_deref(),
        OutputEvent::User(m) => m.base.session_id.as_deref(),
        OutputEvent::ToolCall(m) => m.base.session_id.as_deref(),
        OutputEvent::ToolResult(m) => m.base.session_id.as_deref(),
        OutputEvent::Assistant(m) => m.base.session_id.as_deref(),
        OutputEvent::Interrupt(m) => m.base.session_id.as_deref(),
        OutputEvent::Error(m) => m.base.session_id.as_deref(),
        OutputEvent::Plugin(m) => m.base.session_id.as_deref(),
        OutputEvent::Compression(m) => m.base.session_id.as_deref(),
        OutputEvent::Title(m) => m.base.session_id.as_deref(),
        OutputEvent::Retry(m) => m.base.session_id.as_deref(),
    }
    .unwrap_or("")
}

/// detached child session 的事件由调用方独占消费，不进 fan_out
///
/// 验证项：
/// 1. 调用方持有的 `child_rx` 收到 detached session 的所有事件（事件 session_id == child_id）
/// 2. `App::recv`（fan_out 单一出口）短超时无事件——证明 detached 不 spawn forwarder
#[tokio::test]
async fn detached_child_session_events_reach_caller_exclusively() {
    let (agent_paths, _home) = temp_agent_paths();
    let (registry, _mcp_manager) = build_tool_registry().await;
    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(MockProvider {
            events: text_events("子代理结果"),
        }),
        registry,
        PluginHost::new(),
    )
    .await;
    let app = App::new(engine, None, LogGuard::default());

    // 父 session（占位：让 fan_out 至少有一个 forwarder，但本轮父不 emit）
    let parent_id = app
        .create_session(session_params())
        .await
        .expect("创建父 session 失败");

    // detached 子 session：rx 在调用方手里，**不进 fan_out**（不 spawn forwarder）
    let (child_id, mut child_rx) = app
        .create_detached_child_session(&parent_id, ChildSessionSource::Fresh, session_params())
        .await
        .expect("创建 detached child session 失败");

    // 触发 detached child session 的 ReAct
    app.send(&child_id, guide_msg("子代理任务"))
        .await
        .expect("发消息失败");

    // 验证 1：调用方独占消费 child_rx，收到 detached session 的事件
    let mut got_assistant = false;
    for _ in 0..50 {
        if let Ok(Some(ev)) = timeout(Duration::from_millis(500), child_rx.recv()).await {
            assert_eq!(
                session_id_of(&ev),
                child_id,
                "child_rx 的事件应属于 detached child session"
            );
            if let OutputEvent::Assistant(a) = ev {
                assert!(
                    a.payload
                        .content
                        .as_deref()
                        .unwrap_or_default()
                        .contains("子代理结果"),
                    "Assistant 应含「子代理结果」"
                );
                got_assistant = true;
                break;
            }
        }
    }
    assert!(
        got_assistant,
        "调用方应从 child_rx 收到 detached session 的 Assistant 事件"
    );

    // 验证 2：detached 事件**不**进 fan_out
    //   App::recv 短超时（200ms）：fan_out 此刻应为空（detached 未注册 forwarder）
    //   - Err(超时) 是期望结果：fan_out 为空，证明 detached 事件没进 fan_out
    //   - Ok(Some(ev)) 若发生：session_id 必须不是 child_id（其他 session 串扰也算 fail）
    //   - Ok(None) 不应发生（App 未 shutdown，fan_out_tx 仍在）
    if let Ok(Some(ev)) = timeout(Duration::from_millis(200), app.recv()).await {
        let sid = session_id_of(&ev).to_string();
        assert_ne!(
            sid, child_id,
            "detached child session 的事件不应进 fan_out（App::recv 不应收到），实际收到: {ev:?}"
        );
    }

    app.shutdown().await;
}

/// 对比测试：常规 child session 的事件进 fan_out，App::recv 收到
///
/// 与上一个测试形成对照——证明 `App::create_child_session`（spawn forwarder）
/// 与 `App::create_detached_child_session`（不 spawn）的语义差异。
#[tokio::test]
async fn regular_child_session_events_reach_app_recv() {
    let (agent_paths, _home) = temp_agent_paths();
    let (registry, _mcp_manager) = build_tool_registry().await;
    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(MockProvider {
            events: text_events("子任务回复"),
        }),
        registry,
        PluginHost::new(),
    )
    .await;
    let app = App::new(engine, None, LogGuard::default());

    let parent_id = app
        .create_session(session_params())
        .await
        .expect("创建父 session 失败");

    // 常规 child session：App 内部 spawn forwarder，rx 进 fan_out
    let child_id = app
        .create_child_session(&parent_id, ChildSessionSource::Fresh, session_params())
        .await
        .expect("创建常规 child session 失败");

    app.send(&child_id, guide_msg("子任务"))
        .await
        .expect("发消息失败");

    // App::recv 应收到子 session 的事件（经 forwarder 推到 fan_out）
    let mut got_child_event = false;
    let mut got_assistant = false;
    for _ in 0..50 {
        if let Ok(Some(ev)) = timeout(Duration::from_millis(500), app.recv()).await
            && session_id_of(&ev) == child_id
        {
            got_child_event = true;
            if let OutputEvent::Assistant(a) = ev {
                assert!(
                    a.payload
                        .content
                        .as_deref()
                        .unwrap_or_default()
                        .contains("子任务回复"),
                    "Assistant 应含「子任务回复」"
                );
                got_assistant = true;
                break;
            }
        }
    }
    assert!(
        got_child_event,
        "常规 child session 的事件应进 fan_out（App::recv 收到 child_id 的事件）"
    );
    assert!(got_assistant, "应收到常规 child session 的 Assistant 事件");

    app.shutdown().await;
}

/// detached child session 不在 forward_tasks 表中——end_session 后调用方仍可继续消费 rx
///
/// 验证 detached 的生命周期独立性：调用方独占消费完 rx 自然返 None
/// （session task 退出 → tx_session drop → rx 返 None），不被装配层收尾。
#[tokio::test]
async fn detached_child_session_rx_returns_none_after_session_exits() {
    let (agent_paths, _home) = temp_agent_paths();
    let (registry, _mcp_manager) = build_tool_registry().await;
    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(MockProvider {
            events: text_events("一次性任务"),
        }),
        registry,
        PluginHost::new(),
    )
    .await;
    let app = App::new(engine, None, LogGuard::default());

    let parent_id = app
        .create_session(session_params())
        .await
        .expect("创建父 session 失败");

    let (child_id, mut child_rx) = app
        .create_detached_child_session(&parent_id, ChildSessionSource::Fresh, session_params())
        .await
        .expect("创建 detached child session 失败");

    // 触发 child session 跑完一轮 ReAct
    app.send(&child_id, guide_msg("跑完即退"))
        .await
        .expect("发消息失败");

    // 等收完 Assistant（session task 此时进入 idle，但仍存活）
    let mut got_assistant = false;
    for _ in 0..50 {
        if let Ok(Some(OutputEvent::Assistant(_))) =
            timeout(Duration::from_millis(500), child_rx.recv()).await
        {
            got_assistant = true;
            break;
        }
    }
    assert!(got_assistant, "应从 child_rx 收到 Assistant");

    // 用 end_session 触发 child session task 退出（end_session 不影响 detached——
    // detached 不在 forward_tasks 表，但 engine.end_session 仍能处理 session task）
    app.end_session(&child_id, "detached 任务完成")
        .await
        .expect("end_session 失败");

    // session task 退出 → tx_session drop → child_rx 后续 recv 返 None
    let result = timeout(Duration::from_secs(2), child_rx.recv()).await;
    match result {
        Ok(None) => { /* 期望：session task 退出后 rx 返 None */ }
        Ok(Some(ev)) => {
            panic!("detached session end_session 后 child_rx 应返 None，实际收到事件: {ev:?}")
        }
        Err(_) => panic!("child_rx 在 end_session 后 2 秒未返回（卡住）"),
    }

    app.shutdown().await;
}
