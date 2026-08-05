//! Engine::end_session 单 session 销毁 + 子 session 生命周期
//!
//! 验证 [`Engine`] 自身的 session 生命周期管理，不经过装配层（fuyao-app）。
//! 直连 `engine.create_session` / `create_child_session` 返回的 per-session 通道消费事件。
//!
//! 聚焦点：
//! - end_session 后该 session 从调度表移除（后续 send 返 SessionNotFound）
//! - end_session 落库 ended_at / end_reason
//! - end_session 只销毁指定 session，不波及其他 session
//! - 子 session 的 per-session 通道独立于父 session（事件不串扰）
//! - 子 session task 退出后其 rx 自然返 None

mod common;

use std::time::Duration;

use common::{MockProvider, make_store, temp_agent_paths, text_events};
use fuyao_api::message::input::{UserMessage, UserPayload};
use fuyao_api::message::{EventBase, InputEvent, OutputEvent};
use fuyao_api::{ChildSessionSource, EngineParams, ModelConfig, SessionParams};
use fuyao_core::{Engine, EngineError, PluginHost};
use tokio::time::timeout;

/// 构造 Guide 模式的用户消息（模型配置由 session 的 SessionParams 决定，不随消息走）
fn guide_user_message(content: &str) -> InputEvent {
    InputEvent::User(UserMessage {
        base: EventBase::default(),
        payload: UserPayload {
            content: content.to_string(),
            images: vec![],
            mode: Default::default(),
            source: Default::default(),
        },
    })
}

/// 测试用 SessionParams：携带 `test/model` 形式的 model_id（与各测试的 ProviderRegistry 匹配）
fn test_session_params() -> SessionParams {
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

// ============================================================================
// Engine::end_session：单 session 销毁（与 shutdown 对称但只动一个 child_token）
// ============================================================================

/// end_session 后该 session 从调度表移除——后续 send 返回 `Err(SessionNotFound)`
#[tokio::test]
async fn end_session_removes_from_schedule() {
    let (agent_paths, _home) = temp_agent_paths();
    let store = make_store(&agent_paths).await;
    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(MockProvider {
            events: text_events("ok"),
        }),
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
        store,
    )
    .await;

    let (session_id, _rx_event) = engine
        .create_session(test_session_params())
        .await
        .expect("创建 session 失败");

    // end_session 应正常返回 Ok（task 在 idle 状态，cancel 立即响应退出）
    let done = timeout(
        Duration::from_secs(3),
        engine.end_session(&session_id, "session_ended"),
    )
    .await;
    assert!(done.is_ok(), "end_session 应在 3 秒内完成");
    done.expect("end_session 未在 3s 内完成")
        .expect("end_session 返回错误");

    // end_session 后再 send 应返回 SessionNotFound（session 已从调度表移除）
    let event = guide_user_message("end 后的发送");
    let result = engine.send(&session_id, event).await;
    assert!(
        matches!(result, Err(EngineError::SessionNotFound(_))),
        "end_session 后 send 应返回 Err(SessionNotFound)，实际: {result:?}"
    );

    // 引擎本身仍未 shutdown，可继续创建新 session
    let new_id = engine.create_session(test_session_params()).await;
    assert!(new_id.is_ok(), "end_session 后引擎应仍可创建新 session");

    engine.shutdown().await;
}

/// end_session 把 ended_at / end_reason 写进 DB（task 退出后单字段 UPDATE 落最终值）
#[tokio::test]
async fn end_session_persists_ended_at_and_reason() {
    let (agent_paths, _home) = temp_agent_paths();
    let store = make_store(&agent_paths).await;

    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(MockProvider {
            events: text_events("对话已结束"),
        }),
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
        store,
    )
    .await;

    let (session_id, _rx_event) = engine
        .create_session(test_session_params())
        .await
        .expect("创建 session 失败");

    // end_session 前在 DB 中 ended_at / end_reason 都为 None
    let db_path = agent_paths.sessions_db_path();
    {
        let store = fuyao_session::SessionStore::new(db_path.clone())
            .await
            .expect("打开 store 失败");
        let before = store.get(&session_id).await.unwrap().unwrap();
        assert!(before.ended_at.is_none());
        assert!(before.end_reason.is_none());
    }

    // end_session 应在合理时间内完成
    let done = timeout(
        Duration::from_secs(3),
        engine.end_session(&session_id, "session_ended"),
    )
    .await;
    assert!(done.is_ok(), "end_session 应在 3 秒内完成");
    done.unwrap().expect("end_session 返回错误");

    // DB 验证：ended_at 已落库，end_reason == 传入值
    let store = fuyao_session::SessionStore::new(db_path)
        .await
        .expect("重新打开 store 失败");
    let loaded = store
        .get(&session_id)
        .await
        .expect("DB 查询失败")
        .expect("session 应存在");
    assert!(loaded.ended_at.is_some(), "ended_at 应已落库");
    assert_eq!(
        loaded.end_reason.as_deref(),
        Some("session_ended"),
        "end_reason 应为传入值"
    );

    engine.shutdown().await;
}

/// end_session 只销毁指定 session——其他 session 不受影响，仍可正常 send + recv
///
/// 核心验证：end_session 只 cancel 该 session 的 child_token，不动引擎 root token，
/// 故其他 session 的 task 不会被波及。这是 end_session 与 shutdown 的本质区别。
#[tokio::test]
async fn end_session_does_not_affect_other_sessions() {
    let (agent_paths, _home) = temp_agent_paths();
    let store = make_store(&agent_paths).await;

    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(MockProvider {
            events: text_events("B 的回复"),
        }),
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
        store,
    )
    .await;

    let (session_a, _rx_a) = engine
        .create_session(test_session_params())
        .await
        .expect("创建 session A 失败");
    let (session_b, mut rx_b) = engine
        .create_session(test_session_params())
        .await
        .expect("创建 session B 失败");

    // 销毁 A
    let done = timeout(
        Duration::from_secs(3),
        engine.end_session(&session_a, "session_ended"),
    )
    .await;
    assert!(done.is_ok(), "end_session(A) 应在 3 秒内完成");
    done.unwrap().expect("end_session(A) 返回错误");

    // A 已销毁，再 send A 返回 SessionNotFound
    let event_a = guide_user_message("A 已死");
    let result_a = engine.send(&session_a, event_a).await;
    assert!(
        matches!(result_a, Err(EngineError::SessionNotFound(_))),
        "A 销毁后 send A 应返回 SessionNotFound，实际: {result_a:?}"
    );

    // B 仍正常工作：发消息 + 从 B 的 per-session 通道收到 Chunk / Assistant
    let event_b = guide_user_message("B 还活着");
    engine
        .send(&session_b, event_b)
        .await
        .expect("B 的 send 不应失败");

    let mut got_assistant = false;
    for _ in 0..50 {
        if let Ok(Some(OutputEvent::Assistant(a))) =
            timeout(Duration::from_millis(500), rx_b.recv()).await
        {
            assert!(
                a.payload
                    .content
                    .as_deref()
                    .unwrap_or_default()
                    .contains("B 的回复"),
                "B 的 Assistant 应含「B 的回复」"
            );
            got_assistant = true;
            break;
        }
    }
    assert!(got_assistant, "B 应仍能正常完成 ReAct（end A 不影响 B）");

    engine.shutdown().await;
}

// ============================================================================
// 子 session：per-session 通道独立 + 生命周期
// ============================================================================

/// 子 session 与父 session 各持独立的 per-session 通道，事件互不串扰
///
/// Engine 层面所有 session 都是独立通道（无 fan-in 概念）。本测试验证：
/// 1. 子 session 的 rx_event 只收到自身的事件（session_id == child_id）
/// 2. 父 session 的 rx_event 不收到子 session 的事件（父子通道隔离）
#[tokio::test]
async fn child_session_has_independent_channel_from_parent() {
    let (agent_paths, _home) = temp_agent_paths();
    let store = make_store(&agent_paths).await;
    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(MockProvider {
            events: text_events("子代理结果"),
        }),
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
        store,
    )
    .await;

    // 父 session（持 rx_parent，本轮不发消息，不应收到任何事件）
    let (parent_id, mut rx_parent) = engine
        .create_session(test_session_params())
        .await
        .expect("创建父 session 失败");

    // 子 session：持独立 rx_child
    let (child_id, mut rx_child) = engine
        .create_child_session(&parent_id, ChildSessionSource::Fresh, test_session_params())
        .await
        .expect("创建 child session 失败");

    // 触发子 session 的 ReAct
    engine
        .send(&child_id, guide_user_message("子代理任务"))
        .await
        .expect("发消息失败");

    // 验证 1：子 session 的 rx_child 收到自身事件
    let mut got_assistant = false;
    for _ in 0..50 {
        if let Ok(Some(ev)) = timeout(Duration::from_millis(500), rx_child.recv()).await {
            let sid = match &ev {
                OutputEvent::Chunk(m) => m.base.session_id.as_deref(),
                OutputEvent::Assistant(m) => m.base.session_id.as_deref(),
                OutputEvent::User(m) => m.base.session_id.as_deref(),
                OutputEvent::ToolCall(m) => m.base.session_id.as_deref(),
                OutputEvent::ToolResult(m) => m.base.session_id.as_deref(),
                OutputEvent::Interrupt(m) => m.base.session_id.as_deref(),
                OutputEvent::Error(m) => m.base.session_id.as_deref(),
                OutputEvent::Plugin(m) => m.base.session_id.as_deref(),
                OutputEvent::Compression(m) => m.base.session_id.as_deref(),
                OutputEvent::Title(m) => m.base.session_id.as_deref(),
                OutputEvent::Retry(m) => m.base.session_id.as_deref(),
                OutputEvent::ChildSession(m) => m.base.session_id.as_deref(),
                OutputEvent::Rollback(m) => m.base.session_id.as_deref(),
            }
            .unwrap_or("");
            assert_eq!(
                sid, child_id,
                "子 session 通道的事件应属于 child session，实际: {sid}"
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
    assert!(got_assistant, "子 session 应从自身通道收到 Assistant 事件");

    // 验证 2：父 session 的通道短超时无事件——证明父子通道隔离
    if let Ok(Some(ev)) = timeout(Duration::from_millis(200), rx_parent.recv()).await {
        panic!("父 session 通道不应收到子 session 事件，实际收到: {ev:?}");
    }

    engine.shutdown().await;
}

/// 子 session task 退出后其 rx 自然返 None
///
/// end_session 触发 child session task 退出（task 退出 → tx_session drop → rx 返 None）。
#[tokio::test]
async fn child_session_rx_returns_none_after_session_exits() {
    let (agent_paths, _home) = temp_agent_paths();
    let store = make_store(&agent_paths).await;
    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(MockProvider {
            events: text_events("一次性任务"),
        }),
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
        store,
    )
    .await;

    let (parent_id, _rx_parent) = engine
        .create_session(test_session_params())
        .await
        .expect("创建父 session 失败");

    let (child_id, mut rx_child) = engine
        .create_child_session(&parent_id, ChildSessionSource::Fresh, test_session_params())
        .await
        .expect("创建 child session 失败");

    // 触发 child session 跑完一轮 ReAct
    engine
        .send(&child_id, guide_user_message("跑完即退"))
        .await
        .expect("发消息失败");

    // 等收完 Assistant（session task 此时进入 idle，但仍存活）
    let mut got_assistant = false;
    for _ in 0..50 {
        if let Ok(Some(OutputEvent::Assistant(_))) =
            timeout(Duration::from_millis(500), rx_child.recv()).await
        {
            got_assistant = true;
            break;
        }
    }
    assert!(got_assistant, "应从 child 通道收到 Assistant");

    // end_session 触发 child session task 退出
    engine
        .end_session(&child_id, "child 任务完成")
        .await
        .expect("end_session 失败");

    // session task 退出 → tx_session drop → rx_child 后续 recv 返 None
    let result = timeout(Duration::from_secs(2), rx_child.recv()).await;
    match result {
        Ok(None) => { /* 期望：session task 退出后 rx 返 None */ }
        Ok(Some(ev)) => {
            panic!("child session end_session 后 rx 应返 None，实际收到事件: {ev:?}")
        }
        Err(_) => panic!("child rx 在 end_session 后 2 秒未返回（卡住）"),
    }

    engine.shutdown().await;
}
