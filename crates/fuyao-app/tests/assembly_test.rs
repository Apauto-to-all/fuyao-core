//! fuyao-app 集成测试：装配链路
//!
//! 两个聚焦点：
//! 1. `build_tool_registry`：内置工具（fuyao-tools 静态表）按 `[tools.enabled]` 注入注册表，
//!    断言核心工具 read/write/glob/grep/bash 存在。
//! 2. 最小端到端：MockProvider 驱动引擎跑一轮 ReAct（create_session → send → recv 验证
//!    Chunk + Assistant），证明装配产物可端到端运行。
//!
//! 全程用临时 fuyao_home（隔离 sessions.db），不调 set_config，走 get_config default 兜底
//! （tools.enabled 空 → 全启用；mcp_servers 空 → 不启动 MCP）。

mod common;

use std::time::Duration;

use common::{MockProvider, temp_agent_paths, text_events};
use fuyao_api::message::input::{UserMessage, UserPayload};
use fuyao_api::message::{EventBase, InputEvent, OutputEvent};
use fuyao_api::{EngineParams, MessageParams, ModelConfig, SessionParams};
use fuyao_app::build_tool_registry;
use fuyao_core::{Engine, PluginHost};
use fuyao_provider::{
    BoxStream, ChatRequest, ChatResponse, StreamError, StreamEvent, StreamOptions,
};

/// 构造 Guide 模式的用户消息
fn guide_user_message(content: &str, model_id: &str) -> (InputEvent, MessageParams) {
    let event = InputEvent::User(UserMessage {
        base: EventBase::default(),
        payload: UserPayload {
            content: content.to_string(),
            mode: Default::default(),
            source: Default::default(),
        },
    });
    let params = MessageParams {
        model_config: ModelConfig {
            model_id: Some(model_id.to_string()),
            ..Default::default()
        },
    };
    (event, params)
}

// ============================================================================
// build_tool_registry：内置工具注入
// ============================================================================

#[tokio::test]
async fn build_registry_includes_core_tools() {
    let (registry, mcp_manager) = build_tool_registry().await;

    // 无 mcp 配置 → mcp_manager 为 None
    assert!(mcp_manager.is_none(), "无 MCP 配置时 mcp_manager 应为 None");

    let defs = registry.definitions_json();
    assert!(
        defs.len() >= 5,
        "应注入至少 5 个内置工具，实际：{}",
        defs.len()
    );

    // 序列化后含 function.name，校验核心工具存在
    let names: Vec<String> = defs
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(String::from)
        })
        .collect();
    for expected in ["read", "write", "glob", "grep", "bash"] {
        assert!(
            names.iter().any(|n| n == expected),
            "应注入工具 {expected}，实际：{names:?}"
        );
    }
}

// ============================================================================
// 端到端：Engine::new + build_tool_registry 后可跑 ReAct
// ============================================================================

#[tokio::test]
async fn assembled_engine_runs_react_loop() {
    let (agent_paths, _home) = temp_agent_paths();
    let (registry, _mcp_manager) = build_tool_registry().await;

    let plugin_host = PluginHost::new();
    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(MockProvider {
            events: text_events("装配后回复"),
        }),
        registry,
        plugin_host,
    )
    .await;

    // 创建 session
    let session_id = engine
        .create_session(SessionParams::default())
        .await
        .expect("创建 session 失败");

    // 发一条 guide 消息（触发一轮 ReAct）
    let (event, params) = guide_user_message("装配后测试", "test/model");
    engine
        .send(&session_id, event, params)
        .await
        .expect("发消息失败");

    // 收事件，验证装配后引擎可产出 Chunk + Assistant
    let mut got_chunk = false;
    let mut got_assistant = false;
    for _ in 0..20 {
        match tokio::time::timeout(Duration::from_millis(2000), engine.recv()).await {
            Ok(Some(e)) => match e {
                OutputEvent::Chunk(_) => got_chunk = true,
                OutputEvent::Assistant(a) => {
                    got_assistant = true;
                    assert!(
                        a.payload
                            .content
                            .as_deref()
                            .unwrap_or_default()
                            .contains("装配后回复"),
                        "Assistant 应含装配后回复内容"
                    );
                }
                _ => {}
            },
            _ => break,
        }
        if got_chunk && got_assistant {
            break;
        }
    }

    engine.shutdown().await;

    assert!(got_chunk, "装配后应产出 Chunk 事件");
    assert!(got_assistant, "装配后应产出 Assistant 事件");
}

// ============================================================================
// shutdown：优雅停机串联（engine + MCP）
// ============================================================================

/// shutdown 对空 AppContext（无 MCP manager）不 panic，且消费 ctx drop log_guard
#[tokio::test]
async fn shutdown_with_no_mcp_manager_does_not_panic() {
    let (_, _home) = temp_agent_paths();
    let engine = Engine::new(
        EngineParams {
            agent_paths: fuyao_api::AgentPaths::default(),
        },
        as_providers(MockProvider {
            events: text_events("ok"),
        }),
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
    )
    .await;

    let ctx = fuyao_app::AppContext {
        mcp_manager: None,
        log_guard: fuyao_app::LogGuard::default(),
    };

    // 不应 panic：engine.shutdown() + mcp_manager=None（跳过 stop_all）+ ctx drop
    fuyao_app::shutdown(engine, ctx).await;
}

// ============================================================================
// RetryRunner 端到端：OutputEvent::Retry 在 session 内被发出
// ============================================================================

/// 辅助：把任意 Provider 包成 ProviderRegistry（统一 provider_id="test"）
///
/// 引擎层按消息级 model_id 的 provider_id 部分从 registry 取实例。
/// 本测试所有用例都用 "test/..." 形式的 model_id，统一走 test provider。
fn as_providers<P: fuyao_provider::Provider + 'static>(p: P) -> fuyao_provider::ProviderRegistry {
    fuyao_provider::ProviderRegistry::with_instance("test", std::sync::Arc::new(p))
}

/// 可恢复错误：首次 RateLimit → 应发 OutputEvent::Retry → 重试成功产出 Assistant
///
/// 用 `retry_after_ms: Some(10)` 让退避只睡 10ms（backoff_duration 优先级最高），
/// 避免真睡默认 2 秒初始退避。
#[tokio::test]
async fn retry_runner_emits_retry_event_then_succeeds() {
    let (agent_paths, _home) = temp_agent_paths();
    let (registry, _mcp_manager) = build_tool_registry().await;

    let provider = common::FlakyThenSuccessProvider::new(
        vec![fuyao_provider::StreamError::RateLimit {
            retry_after_ms: Some(10),
            retry_after_secs: None,
        }],
        text_events("重试后成功"),
    );

    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(provider),
        registry,
        PluginHost::new(),
    )
    .await;

    let session_id = engine
        .create_session(SessionParams::default())
        .await
        .expect("创建 session 失败");

    let (event, params) = guide_user_message("测试重试", "test/model");
    engine
        .send(&session_id, event, params)
        .await
        .expect("发消息失败");

    // 收事件：期望顺序 Retry(attempt=1) → Chunk → Assistant
    let mut got_retry = false;
    let mut got_assistant = false;
    for _ in 0..50 {
        match tokio::time::timeout(Duration::from_millis(2000), engine.recv()).await {
            Ok(Some(OutputEvent::Retry(r))) => {
                got_retry = true;
                assert_eq!(r.payload.attempt, 1, "首次重试 attempt 应为 1");
                assert_eq!(r.payload.max_retries, u32::MAX, "默认无限重试");
                assert_eq!(
                    r.payload.wait_ms, 10,
                    "退避应等于 retry-after-ms 头（优先级最高）"
                );
                assert!(
                    r.payload.cause.contains("速率限制"),
                    "cause 应含错误描述，实际: {}",
                    r.payload.cause
                );
            }
            Ok(Some(OutputEvent::Assistant(a))) => {
                got_assistant = true;
                assert!(
                    a.payload
                        .content
                        .as_deref()
                        .unwrap_or_default()
                        .contains("重试后成功"),
                    "Assistant 应含重试后成功内容"
                );
                break;
            }
            _ => {}
        }
    }

    engine.shutdown().await;
    assert!(got_retry, "应发出 OutputEvent::Retry 事件");
    assert!(got_assistant, "重试后应产出 Assistant 事件");
}

/// 严重错误（不可重试）：AuthError → 不应发 Retry 事件，Error 立即冒泡
#[tokio::test]
async fn retry_runner_no_retry_on_auth_error() {
    let (agent_paths, _home) = temp_agent_paths();
    let (registry, _mcp_manager) = build_tool_registry().await;

    let provider = common::FlakyThenSuccessProvider::new(
        vec![fuyao_provider::StreamError::AuthError(
            "invalid key".to_string(),
        )],
        text_events("永远不该被看到"),
    );

    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(provider),
        registry,
        PluginHost::new(),
    )
    .await;

    let session_id = engine
        .create_session(SessionParams::default())
        .await
        .expect("创建 session 失败");

    let (event, params) = guide_user_message("测试严重错误", "test/model");
    engine
        .send(&session_id, event, params)
        .await
        .expect("发消息失败");

    // 收事件：应有 Error，不应有 Retry
    let mut got_error = false;
    let mut got_retry = false;
    for _ in 0..50 {
        match tokio::time::timeout(Duration::from_millis(2000), engine.recv()).await {
            Ok(Some(OutputEvent::Error(_))) => {
                got_error = true;
                break;
            }
            Ok(Some(OutputEvent::Retry(_))) => {
                got_retry = true;
            }
            _ => {}
        }
    }

    engine.shutdown().await;
    assert!(!got_retry, "严重错误不应触发 Retry 事件");
    assert!(got_error, "严重错误应冒泡为 OutputEvent::Error");
}

/// 首 chunk 后错误：吐 TextDelta 后再吐 RateLimit → 不应发 Retry，Error 冒泡
#[tokio::test]
async fn retry_runner_no_retry_after_first_chunk() {
    let (agent_paths, _home) = temp_agent_paths();
    let (registry, _mcp_manager) = build_tool_registry().await;

    // 构造一个始终吐「TextDelta + RateLimit 错误」的 Provider
    struct AlwaysFirstChunkThenError;
    #[async_trait::async_trait]
    impl fuyao_provider::Provider for AlwaysFirstChunkThenError {
        fn stream_chat(
            &self,
            _request: ChatRequest,
            _model: &str,
            _options: StreamOptions,
        ) -> BoxStream<Result<StreamEvent, StreamError>> {
            let items: Vec<Result<StreamEvent, StreamError>> = vec![
                Ok(StreamEvent::TextDelta {
                    content: "hi".to_string(),
                }),
                Err(fuyao_provider::StreamError::RateLimit {
                    retry_after_ms: Some(10),
                    retry_after_secs: None,
                }),
            ];
            Box::pin(futures_util::stream::iter(items))
        }

        async fn chat(
            &self,
            _request: ChatRequest,
            _model: &str,
        ) -> Result<ChatResponse, StreamError> {
            Err(StreamError::ApiError("mock: chat 不支持".to_string()))
        }
    }

    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(AlwaysFirstChunkThenError),
        registry,
        PluginHost::new(),
    )
    .await;

    let session_id = engine
        .create_session(SessionParams::default())
        .await
        .expect("创建 session 失败");

    let (event, params) = guide_user_message("测试首 chunk", "test/model");
    engine
        .send(&session_id, event, params)
        .await
        .expect("发消息失败");

    let mut got_error = false;
    let mut got_retry = false;
    for _ in 0..50 {
        match tokio::time::timeout(Duration::from_millis(2000), engine.recv()).await {
            Ok(Some(OutputEvent::Error(_))) => {
                got_error = true;
                break;
            }
            Ok(Some(OutputEvent::Retry(_))) => {
                got_retry = true;
            }
            _ => {}
        }
    }

    engine.shutdown().await;
    assert!(
        !got_retry,
        "首 chunk 后错误不应触发 Retry（避免 UI 重复输出）"
    );
    assert!(got_error, "首 chunk 后错误应冒泡为 Error");
}

/// 重试循环持续发 Retry 事件：连返 RateLimit → 至少发 1 条 Retry 事件
///
/// 默认 `max_retries=u32::MAX`（无限重试），不会自然耗尽。测试用 deadline
/// 打断循环，只验证「Retry 事件确实持续被发出」——max_retries=0 的耗尽场景
/// 由 fuyao-core 单测覆盖（带可配置 max_retries 的精确断言）。
#[tokio::test]
async fn retry_runner_emits_multiple_retry_events_under_persistent_error() {
    let (agent_paths, _home) = temp_agent_paths();
    let (registry, _mcp_manager) = build_tool_registry().await;

    // 构造永远失败的 Provider（每次都返带 retry-after-ms=10 的 RateLimit）
    struct AlwaysRateLimit;
    #[async_trait::async_trait]
    impl fuyao_provider::Provider for AlwaysRateLimit {
        fn stream_chat(
            &self,
            _request: ChatRequest,
            _model: &str,
            _options: StreamOptions,
        ) -> BoxStream<Result<StreamEvent, StreamError>> {
            let e = fuyao_provider::StreamError::RateLimit {
                retry_after_ms: Some(10),
                retry_after_secs: None,
            };
            Box::pin(futures_util::stream::iter(std::iter::once(Err(e))))
        }

        async fn chat(
            &self,
            _request: ChatRequest,
            _model: &str,
        ) -> Result<ChatResponse, StreamError> {
            Err(StreamError::ApiError("mock: chat 不支持".to_string()))
        }
    }

    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(AlwaysRateLimit),
        registry,
        PluginHost::new(),
    )
    .await;

    let session_id = engine
        .create_session(SessionParams::default())
        .await
        .expect("创建 session 失败");

    let (event, params) = guide_user_message("测试持续重试", "test/model");
    engine
        .send(&session_id, event, params)
        .await
        .expect("发消息失败");

    // 2 秒内应至少看到 2 条 Retry 事件（attempt 递增）
    let mut retry_attempts = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(2000);
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        if let Ok(Some(OutputEvent::Retry(r))) =
            tokio::time::timeout(Duration::from_millis(200), engine.recv()).await
        {
            retry_attempts.push(r.payload.attempt);
        }
    }

    engine.shutdown().await;
    assert!(
        retry_attempts.len() >= 2,
        "持续错误应至少触发 2 次 Retry 事件（实际: {} 次）",
        retry_attempts.len()
    );
    // attempt 应递增（1, 2, 3, ...）
    assert_eq!(retry_attempts[0], 1, "首次 attempt 应为 1");
    assert!(
        retry_attempts.windows(2).all(|w| w[1] == w[0] + 1),
        "attempt 应递增 1, 2, 3, ...，实际: {retry_attempts:?}"
    );
}

// ============================================================================
// Engine::shutdown 完整流程：flag 拒绝 / recv 返回 None / 落库 / task 退出
// ============================================================================

/// shutdown 后 send 立即返回 `Err(EngineError::Shutdown)`（而非 SessionNotFound）
#[tokio::test]
async fn shutdown_blocks_send_with_shutdown_error() {
    let (agent_paths, _home) = temp_agent_paths();
    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(MockProvider {
            events: text_events("ok"),
        }),
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
    )
    .await;

    let session_id = engine
        .create_session(SessionParams::default())
        .await
        .expect("创建 session 失败");

    engine.shutdown().await;

    // shutdown 后 send 应立即返回 Shutdown 错误
    let (event, params) = guide_user_message("shutdown 后的发送", "test/model");
    let result = engine.send(&session_id, event, params).await;
    assert!(
        matches!(result, Err(fuyao_core::EngineError::Shutdown)),
        "shutdown 后 send 应返回 Err(Shutdown)，实际: {result:?}"
    );
}

/// shutdown 后 recv 返回 None（在 drain 完残余事件后）
#[tokio::test]
async fn shutdown_returns_none_for_recv() {
    let (agent_paths, _home) = temp_agent_paths();
    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(MockProvider {
            events: text_events("ok"),
        }),
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
    )
    .await;

    let _session_id = engine
        .create_session(SessionParams::default())
        .await
        .expect("创建 session 失败");

    // 不发消息，直接 shutdown；task 在 select! 收到 cancelled 后退出
    engine.shutdown().await;

    // recv 应最终返回 None（shutdown 后通道 drain 完）
    let result = tokio::time::timeout(Duration::from_secs(2), engine.recv()).await;
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
    let (registry, _mcp_manager) = build_tool_registry().await;

    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(MockProvider {
            events: text_events("对话已结束"),
        }),
        registry,
        PluginHost::new(),
    )
    .await;

    let session_id = engine
        .create_session(SessionParams::default())
        .await
        .expect("创建 session 失败");

    let (event, params) = guide_user_message("你好", "test/model");
    engine
        .send(&session_id, event, params)
        .await
        .expect("发消息失败");

    // 收到 Assistant 事件（证明 ReAct 跑完了）
    let mut got_assistant = false;
    for _ in 0..50 {
        if let Ok(Some(OutputEvent::Assistant(a))) =
            tokio::time::timeout(Duration::from_millis(500), engine.recv()).await
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
    let shutdown_done = tokio::time::timeout(Duration::from_secs(3), engine.shutdown()).await;
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
    let (registry, _mcp_manager) = build_tool_registry().await;

    // 持续 RateLimit（retry_after_ms 设大，模拟退避 sleep 中——故意远超 shutdown 超时阈值）
    let provider = common::FlakyThenSuccessProvider::new(
        vec![fuyao_provider::StreamError::RateLimit {
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
        registry,
        PluginHost::new(),
    )
    .await;

    let session_id = engine
        .create_session(SessionParams::default())
        .await
        .expect("创建 session 失败");

    let (event, params) = guide_user_message("触发持续重试", "test/model");
    engine
        .send(&session_id, event, params)
        .await
        .expect("发消息失败");

    // 等收到一个 Retry 事件，确认进入退避 sleep
    let mut entered_backoff = false;
    for _ in 0..20 {
        if let Ok(Some(OutputEvent::Retry(_))) =
            tokio::time::timeout(Duration::from_millis(500), engine.recv()).await
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
    let shutdown_done = tokio::time::timeout(Duration::from_secs(2), engine.shutdown()).await;
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
    impl fuyao_provider::Provider for AlwaysRateLimitProvider {
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
        ) -> Result<ChatResponse, StreamError> {
            Err(StreamError::ApiError("mock: chat 不支持".into()))
        }
    }

    let (agent_paths, _home) = temp_agent_paths();
    let (registry, _mcp_manager) = build_tool_registry().await;

    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(AlwaysRateLimitProvider),
        registry,
        PluginHost::new(),
    )
    .await;

    // 创建 5 个并发 session，每个发一条消息触发 retry 退避
    const N: usize = 5;
    let mut session_ids = Vec::with_capacity(N);
    for i in 0..N {
        let id = engine
            .create_session(SessionParams::default())
            .await
            .expect("创建 session 失败");
        let (event, params) = guide_user_message(&format!("触发重试 #{i}"), "test/model");
        engine.send(&id, event, params).await.expect("发消息失败");
        session_ids.push(id);
    }

    // 等 5 个 session 都至少收到一个 Retry 事件，确认都进入退避 sleep
    // （retry_after_ms=30s，sleep 中 task 不会自然退出）
    let mut entered_backoff_count = 0;
    for _ in 0..200 {
        if let Ok(Some(OutputEvent::Retry(_))) =
            tokio::time::timeout(Duration::from_millis(200), engine.recv()).await
        {
            entered_backoff_count += 1;
            if entered_backoff_count >= N {
                break;
            }
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
    let shutdown_done = tokio::time::timeout(Duration::from_secs(5), engine.shutdown()).await;
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
