//! Engine::shutdown 完整流程：flag 拒绝 / recv 返回 None / 落库 / task 退出
//!
//! 验证 [`Engine`] 自身的 shutdown 语义，不经过装配层（fuyao-app）。
//! 直连 `engine.create_session` 返回的 per-session 通道消费事件。
//!
//! 聚焦点：
//! - shutdown 后 send 立即返回 `Err(EngineError::Shutdown)`
//! - shutdown 后 per-session rx 返回 None（通道 drain 完）
//! - shutdown 中断活跃 task + 落库验证
//! - shutdown 打断 retry 退避中的 task
//! - 多 session 并发 shutdown 是并发退出而非串行

mod common;

use std::time::Duration;

use common::{FlakyThenSuccessProvider, MockProvider, make_store, temp_agent_paths, text_events};
use fuyao_api::message::input::{UserMessage, UserPayload};
use fuyao_api::message::{EventBase, InputEvent, OutputEvent};
use fuyao_api::{EngineParams, ModelConfig, SessionParams};
use fuyao_core::{Engine, EngineError, PluginHost};
use fuyao_provider::{
    BoxStream, ChatRequest, ChatResponse, Provider, StreamError, StreamEvent, StreamOptions,
};
use tokio::sync::mpsc;
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
fn as_providers<P: Provider + 'static>(p: P) -> fuyao_provider::ProviderRegistry {
    fuyao_provider::ProviderRegistry::with_instance("test", std::sync::Arc::new(p))
}

/// shutdown 后 send 立即返回 `Err(EngineError::Shutdown)`（而非 SessionNotFound）
#[tokio::test]
async fn shutdown_blocks_send_with_shutdown_error() {
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

    engine.shutdown().await;

    // shutdown 后 send 应立即返回 Shutdown 错误
    let event = guide_user_message("shutdown 后的发送");
    let result = engine.send(&session_id, event).await;
    assert!(
        matches!(result, Err(EngineError::Shutdown)),
        "shutdown 后 send 应返回 Err(Shutdown)，实际: {result:?}"
    );
}

/// shutdown 后 per-session rx 返回 None（在 drain 完残余事件后）
#[tokio::test]
async fn shutdown_returns_none_for_recv() {
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

    let (_session_id, mut rx_event) = engine
        .create_session(test_session_params())
        .await
        .expect("创建 session 失败");

    // 不发消息，直接 shutdown；task 在 select! 收到 cancelled 后退出
    engine.shutdown().await;

    // recv 应最终返回 None（shutdown 后通道 drain 完）
    let result = timeout(Duration::from_secs(2), rx_event.recv()).await;
    match result {
        Ok(None) => { /* 期望：返回 None */ }
        Ok(Some(ev)) => panic!("shutdown 后 recv 应返回 None，实际收到事件: {ev:?}"),
        Err(_) => panic!("recv 在 shutdown 后 2 秒未返回（卡住）"),
    }
}

/// shutdown 把活跃 session task 的退出路径覆盖——
/// 通过「正常跑完一轮对话 + shutdown」验证 task 优雅退出 + 落库
#[tokio::test]
async fn shutdown_terminates_active_session_and_persists() {
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

    let (session_id, mut rx_event) = engine
        .create_session(test_session_params())
        .await
        .expect("创建 session 失败");

    engine
        .send(&session_id, guide_user_message("你好"))
        .await
        .expect("发消息失败");

    // 收到 Assistant 事件（证明 ReAct 跑完了）
    let mut got_assistant = false;
    for _ in 0..50 {
        if let Ok(Some(OutputEvent::Assistant(a))) =
            timeout(Duration::from_millis(500), rx_event.recv()).await
        {
            assert!(
                a.payload
                    .content
                    .as_deref()
                    .unwrap_or_default()
                    .contains("对话已结束"),
                "Assistant 应含「对话已结束」"
            );
            got_assistant = true;
            break;
        }
    }
    assert!(got_assistant, "应收到 Assistant 事件");

    // shutdown：此时 task 应在 idle（turn 跑完后），select! 立即响应 cancelled 退出
    let shutdown_done = timeout(Duration::from_secs(3), engine.shutdown()).await;
    assert!(
        shutdown_done.is_ok(),
        "shutdown 应在 3 秒内完成（task 应立即响应 cancelled 退出）"
    );

    // 从 DB 验证落库（user + assistant 两条消息）
    let db_path = agent_paths.sessions_db_path();
    let store = fuyao_session::SessionStore::new(db_path)
        .await
        .expect("重新打开 store 失败");
    let persisted_msgs = store
        .load_visible_messages(&session_id)
        .await
        .expect("DB 查询失败");
    assert!(
        persisted_msgs.len() >= 2,
        "DB 中应至少有 user + assistant 两条消息，实际: {}",
        persisted_msgs.len()
    );
}

/// 持续错误的重试场景下 shutdown 不卡——
/// 验证 task 在「正在 retry 退避」时 shutdown 也能立即退出
#[tokio::test]
async fn shutdown_unblocks_task_in_retry_backoff() {
    let (agent_paths, _home) = temp_agent_paths();
    let store = make_store(&agent_paths).await;

    // 持续 RateLimit（retry_after_ms 设大，模拟退避 sleep 中——故意远超 shutdown 超时阈值）
    let provider = FlakyThenSuccessProvider::new(
        vec![StreamError::RateLimit {
            retry_after_ms: Some(30000),
            retry_after_secs: None,
        }],
        text_events("永远到不了"),
    );

    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(provider),
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
        store,
    )
    .await;

    let (session_id, mut rx_event) = engine
        .create_session(test_session_params())
        .await
        .expect("创建 session 失败");

    engine
        .send(&session_id, guide_user_message("触发持续重试"))
        .await
        .expect("发消息失败");

    // 等收到一个 Retry 事件，确认进入退避 sleep
    let mut entered_backoff = false;
    for _ in 0..20 {
        if let Ok(Some(OutputEvent::Retry(_))) =
            timeout(Duration::from_millis(500), rx_event.recv()).await
        {
            entered_backoff = true;
            break;
        }
    }
    assert!(entered_backoff, "应至少收到一个 Retry 事件（已进入退避）");

    // shutdown：task 此刻在 retry 的退避 sleep 中（30s 长 sleep）
    // retry.rs 的 sleep 用 select! 监听 shutdown_token，收到信号立即冒泡 Cancelled；
    // turn.rs 流式 select! 的 shutdown 分支（biased 优先）接管，落库退出。
    // 断言 2s 内完成——证明不依赖退避 sleep 走完、也不依赖 10s abort 兜底。
    let shutdown_done = timeout(Duration::from_secs(2), engine.shutdown()).await;
    assert!(
        shutdown_done.is_ok(),
        "shutdown 应在 2 秒内完成（retry sleep 监听 shutdown_token 立即冒泡 Cancelled，turn.rs shutdown 分支接管退出）"
    );
}

/// 多 session 并发活跃时 shutdown 是**并发**等待退出，而非串行
///
/// 场景：5 个 session 同时进入 retry 退避 sleep（retry_after_ms=30000，远超 SHUTDOWN_TASK_TIMEOUT）。
/// 修复前（串行 await）：每个 task 独立 10s 超时，总耗时 ≈ 5 × 10s = 50s
/// 修复后（JoinSet 并发 + 总 10s 超时）：所有 task 共享 10s 预算，sleep 同时被 shutdown_token 取消，
/// 总耗时应远小于 10s（通常毫秒级）。
///
/// 断言阈值 5 秒——既验证并发（远小于串行的 50s），又留足 CI 抖动余量。
#[tokio::test]
async fn shutdown_terminates_concurrent_sessions_in_parallel() {
    /// 始终返回 RateLimit 错误流的 Provider（每次 stream_chat 都失败）
    /// ——用于让任意数量的 session 都持续进入 retry 退避（共享 Provider 实例也能并发触发）
    struct AlwaysRateLimitProvider;
    #[async_trait::async_trait]
    impl Provider for AlwaysRateLimitProvider {
        fn stream_chat(
            &self,
            _request: ChatRequest,
            _model: &str,
            _options: StreamOptions,
        ) -> BoxStream<Result<StreamEvent, StreamError>> {
            // 每次调用都返回 RateLimit（retry_after_ms=30s 远超 shutdown 预算）
            let item: Result<StreamEvent, StreamError> = Err(StreamError::RateLimit {
                retry_after_ms: Some(30000),
                retry_after_secs: None,
            });
            Box::pin(futures_util::stream::iter(std::iter::once(item)))
        }
        async fn chat(
            &self,
            _request: ChatRequest,
            _model: &str,
            _options: StreamOptions,
        ) -> Result<ChatResponse, StreamError> {
            Err(StreamError::ApiError("mock: chat 不支持".into()))
        }
    }

    let (agent_paths, _home) = temp_agent_paths();
    let store = make_store(&agent_paths).await;

    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(AlwaysRateLimitProvider),
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
        store,
    )
    .await;

    // 创建 5 个并发 session，每个发一条消息触发 retry 退避
    // 每个 session 的 per-session rx_event 独立持有——core 无 fan-in，用 mpsc
    // 把 5 个 rx 的 Retry 事件汇聚到一处统一计数
    const N: usize = 5;
    let mut session_ids = Vec::with_capacity(N);
    let mut rxs = Vec::with_capacity(N);
    for i in 0..N {
        let (id, rx_event) = engine
            .create_session(test_session_params())
            .await
            .expect("创建 session 失败");
        engine
            .send(&id, guide_user_message(&format!("触发重试 #{i}")))
            .await
            .expect("发消息失败");
        session_ids.push(id);
        rxs.push(rx_event);
    }

    // 把 5 个 per-session rx 转发到一个汇聚 mpsc，统一消费 Retry 事件计数
    let (agg_tx, mut agg_rx) = mpsc::channel::<OutputEvent>(64);
    for mut rx in rxs {
        let agg_tx = agg_tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if agg_tx.send(ev).await.is_err() {
                    break;
                }
            }
        });
    }

    // 等 5 个 session 都至少收到一个 Retry 事件，确认都进入退避 sleep
    // （retry_after_ms=30s，sleep 中 task 不会自然退出）
    let mut entered_backoff_count = 0;
    let collect_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if entered_backoff_count >= N || tokio::time::Instant::now() >= collect_deadline {
            break;
        }
        if let Ok(Some(OutputEvent::Retry(_))) =
            timeout(Duration::from_millis(200), agg_rx.recv()).await
        {
            entered_backoff_count += 1;
        }
    }
    assert_eq!(
        entered_backoff_count, N,
        "应有 {N} 个 session 都进入 retry 退避"
    );

    // shutdown：5 个 task 都在 retry 30s sleep 中
    // 串行模式（旧）会退化到 ≈ N × 30s（实际被 abort 在 N × SHUTDOWN_TASK_TIMEOUT）
    // 并发模式（新）应远小于 SHUTDOWN_TASK_TIMEOUT（10s）——所有 sleep 同时被 cancel
    let start = std::time::Instant::now();
    let shutdown_done = timeout(Duration::from_secs(5), engine.shutdown()).await;
    let elapsed = start.elapsed();

    assert!(
        shutdown_done.is_ok(),
        "shutdown 应在 5 秒内完成（5 个 task 并发退出，远小于串行的 N × 30s），实际未完成"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "shutdown 耗时应 < 5s（并发退出），实际: {elapsed:?}"
    );

    // 验证 5 个 session 的元数据都落库了（即使被中断，session 元数据仍应有记录）
    let db_path = agent_paths.sessions_db_path();
    let store = fuyao_session::SessionStore::new(db_path)
        .await
        .expect("重新打开 store 失败");
    for id in &session_ids {
        let session = store.get(id).await.expect("DB 查询失败");
        assert!(session.is_some(), "session {id} 应在 DB 中存在");
    }
}
