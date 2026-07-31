//! ReAct 循环核心行为：retry 机制 + 基础端到端
//!
//! 验证 [`Engine`] 自身的 ReAct 调度逻辑，不经过装配层（fuyao-app）的 fan-in。
//! 直连 `engine.create_session` 返回的 per-session 通道消费事件。
//!
//! 聚焦点：
//! - 基础端到端：MockProvider 驱动跑一轮 ReAct，验证 Chunk + Assistant 事件链
//! - RetryRunner：可恢复错误触发 Retry 事件并重试成功 / 不可恢复错误立即冒泡 /
//!   首 chunk 后错误不重试 / 持续错误持续发 Retry 事件

mod common;

use std::time::Duration;

use common::{FlakyThenSuccessProvider, MockProvider, temp_agent_paths, text_events};
use fuyao_api::message::input::{UserMessage, UserPayload};
use fuyao_api::message::{EventBase, InputEvent, OutputEvent};
use fuyao_api::{EngineParams, ModelConfig, SessionParams};
use fuyao_core::{Engine, EngineError, PluginHost};
use fuyao_provider::{
    BoxStream, ChatRequest, ChatResponse, Provider, StreamError, StreamEvent, StreamOptions,
};

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
///
/// 引擎层按消息级 model_id 的 provider_id 部分从 registry 取实例。
/// 本测试所有用例都用 "test/..." 形式的 model_id，统一走 test provider。
fn as_providers<P: Provider + 'static>(p: P) -> fuyao_provider::ProviderRegistry {
    fuyao_provider::ProviderRegistry::with_instance("test", std::sync::Arc::new(p))
}

// ============================================================================
// 基础端到端：Engine 直接驱动一轮 ReAct
// ============================================================================

/// Engine 直连跑一轮 ReAct：send → rx_event 收到 Chunk + Assistant
///
/// 不经装配层，验证引擎核心自 create_session 返回的 per-session 通道能完整产出事件链。
#[tokio::test]
async fn engine_runs_react_loop_on_direct_channel() {
    let (agent_paths, _home) = temp_agent_paths();

    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(MockProvider {
            events: text_events("引擎直连回复"),
        }),
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
    )
    .await;

    let (session_id, mut rx_event) = engine
        .create_session(test_session_params())
        .await
        .expect("创建 session 失败");

    engine
        .send(&session_id, guide_user_message("直连测试"))
        .await
        .expect("发消息失败");

    // 收事件（直连 per-session 通道），验证 Chunk + Assistant
    let mut got_chunk = false;
    let mut got_assistant = false;
    for _ in 0..50 {
        match tokio::time::timeout(Duration::from_millis(2000), rx_event.recv()).await {
            Ok(Some(OutputEvent::Chunk(_))) => got_chunk = true,
            Ok(Some(OutputEvent::Assistant(a))) => {
                got_assistant = true;
                assert!(
                    a.payload
                        .content
                        .as_deref()
                        .unwrap_or_default()
                        .contains("引擎直连回复"),
                    "Assistant 应含引擎直连回复内容"
                );
            }
            // 忽略 User 回显 / Title 等非目标事件，继续等待
            Ok(Some(_)) => continue,
            _ => break,
        }
        if got_chunk && got_assistant {
            break;
        }
    }

    engine.shutdown().await;

    assert!(got_chunk, "引擎应产出 Chunk 事件");
    assert!(got_assistant, "引擎应产出 Assistant 事件");
}

// ============================================================================
// RetryRunner：可恢复错误的重试行为
// ============================================================================

/// 可恢复错误：首次 RateLimit → 应发 OutputEvent::Retry → 重试成功产出 Assistant
///
/// 用 `retry_after_ms: Some(10)` 让退避只睡 10ms（backoff_duration 优先级最高），
/// 避免真睡默认 2 秒初始退避。
#[tokio::test]
async fn retry_emits_retry_event_then_succeeds() {
    let (agent_paths, _home) = temp_agent_paths();

    let provider = FlakyThenSuccessProvider::new(
        vec![StreamError::RateLimit {
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
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
    )
    .await;

    let (session_id, mut rx_event) = engine
        .create_session(test_session_params())
        .await
        .expect("创建 session 失败");

    engine
        .send(&session_id, guide_user_message("测试重试"))
        .await
        .expect("发消息失败");

    // 收事件：期望顺序 Retry(attempt=1) → Chunk → Assistant
    let mut got_retry = false;
    let mut got_assistant = false;
    for _ in 0..50 {
        match tokio::time::timeout(Duration::from_millis(2000), rx_event.recv()).await {
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
async fn retry_no_retry_on_auth_error() {
    let (agent_paths, _home) = temp_agent_paths();

    let provider = FlakyThenSuccessProvider::new(
        vec![StreamError::AuthError("invalid key".to_string())],
        text_events("永远不该被看到"),
    );

    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(provider),
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
    )
    .await;

    let (session_id, mut rx_event) = engine
        .create_session(test_session_params())
        .await
        .expect("创建 session 失败");

    engine
        .send(&session_id, guide_user_message("测试严重错误"))
        .await
        .expect("发消息失败");

    // 收事件：应有 Error，不应有 Retry
    let mut got_error = false;
    let mut got_retry = false;
    for _ in 0..50 {
        match tokio::time::timeout(Duration::from_millis(2000), rx_event.recv()).await {
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

    assert!(got_error, "AuthError 应立即冒泡为 Error 事件");
    assert!(!got_retry, "AuthError 不可重试，不应发 Retry 事件");
}

/// 首 chunk 后的错误不触发重试：避免 UI 重复输出已开始的内容
///
/// 构造一个「TextDelta + RateLimit」的流：首 chunk 已发出，后续错误不重试，
/// 直接冒泡为 Error（重试会丢已输出的增量，UI 看到半截内容后重复输出）。
#[tokio::test]
async fn retry_no_retry_after_first_chunk() {
    let (agent_paths, _home) = temp_agent_paths();

    // 构造一个始终吐「TextDelta + RateLimit 错误」的 Provider
    struct AlwaysFirstChunkThenError;
    #[async_trait::async_trait]
    impl Provider for AlwaysFirstChunkThenError {
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
                Err(StreamError::RateLimit {
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
            _options: StreamOptions,
        ) -> Result<ChatResponse, StreamError> {
            Err(StreamError::ApiError("mock: chat 不支持".to_string()))
        }
    }

    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(AlwaysFirstChunkThenError),
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
    )
    .await;

    let (session_id, mut rx_event) = engine
        .create_session(test_session_params())
        .await
        .expect("创建 session 失败");

    engine
        .send(&session_id, guide_user_message("测试首 chunk"))
        .await
        .expect("发消息失败");

    let mut got_error = false;
    let mut got_retry = false;
    for _ in 0..50 {
        match tokio::time::timeout(Duration::from_millis(2000), rx_event.recv()).await {
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

/// 重试循环持续发 Retry 事件：连返 RateLimit → 至少发 2 条 attempt 递增的 Retry 事件
///
/// 默认 `max_retries=u32::MAX`（无限重试），不会自然耗尽。测试用 deadline
/// 打断循环，只验证「Retry 事件确实持续被发出」——max_retries=0 的耗尽场景
/// 由 fuyao-core 单测覆盖（带可配置 max_retries 的精确断言）。
#[tokio::test]
async fn retry_emits_multiple_retry_events_under_persistent_error() {
    let (agent_paths, _home) = temp_agent_paths();

    // 构造永远失败的 Provider（每次都返带 retry-after-ms=10 的 RateLimit）
    struct AlwaysRateLimit;
    #[async_trait::async_trait]
    impl Provider for AlwaysRateLimit {
        fn stream_chat(
            &self,
            _request: ChatRequest,
            _model: &str,
            _options: StreamOptions,
        ) -> BoxStream<Result<StreamEvent, StreamError>> {
            let e = StreamError::RateLimit {
                retry_after_ms: Some(10),
                retry_after_secs: None,
            };
            Box::pin(futures_util::stream::iter(std::iter::once(Err(e))))
        }

        async fn chat(
            &self,
            _request: ChatRequest,
            _model: &str,
            _options: StreamOptions,
        ) -> Result<ChatResponse, StreamError> {
            Err(StreamError::ApiError("mock: chat 不支持".to_string()))
        }
    }

    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(AlwaysRateLimit),
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
    )
    .await;

    let (session_id, mut rx_event) = engine
        .create_session(test_session_params())
        .await
        .expect("创建 session 失败");

    engine
        .send(&session_id, guide_user_message("测试持续重试"))
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
            tokio::time::timeout(Duration::from_millis(200), rx_event.recv()).await
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

/// 主动验证：send 到不存在的 session 返回 SessionNotFound（Engine 公开 API 契约）
#[tokio::test]
async fn send_to_unknown_session_returns_not_found() {
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

    let unknown_id = String::from("不存在的id");
    let result = engine.send(&unknown_id, guide_user_message("x")).await;
    assert!(
        matches!(result, Err(EngineError::SessionNotFound(_))),
        "send 到不存在的 session 应返回 SessionNotFound，实际: {result:?}"
    );

    engine.shutdown().await;
}
