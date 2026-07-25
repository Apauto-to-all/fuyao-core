//! fuyao-provider HTTP 集成测试（mockito）
//!
//! 用 mockito 起本地 HTTP server，测 OpenAIProvider 的真实 HTTP 行为：
//! - stream_chat 流式 SSE 解析（多 chunk、跨 chunk UTF-8、Done 触发）
//! - chat 非流式响应解析（content/reasoning/usage/tool_calls）
//! - HTTP 错误分类（401→AuthError、429→RateLimit、400→ContextOverflow、500→ApiError）
//!
//! 这是本 crate 的最大增量价值点：stream_chat / chat / send_request 在单元测试中零覆盖。
//! 用 from_parts 注入 mockito server URL，绕开全局 config 与真实网络。

use futures_util::StreamExt;
use fuyao_api::MessageRole;
use fuyao_provider::{
    ChatMessage, ChatRequest, FinishReason, OpenAIProvider, Provider, StreamError, StreamEvent,
    StreamOptions,
};
use mockito::Server;

/// 构造指向 mockito server 的 provider，base_url 为 `{server}/v1`
fn mock_provider(server: &Server) -> OpenAIProvider {
    OpenAIProvider::from_parts(
        "test-key".to_string(),
        format!("{}/v1", server.url()),
        reqwest::Client::new(),
    )
}

/// 构造最小请求（单条 user 消息）
fn simple_request(content: &str) -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage {
            role: MessageRole::User,
            content: Some(content.to_string()),
            ..Default::default()
        }],
        system: None,
    }
}

/// 消费 stream_chat 的全部事件
async fn collect_stream_events(
    provider: OpenAIProvider,
    request: ChatRequest,
) -> Result<Vec<StreamEvent>, StreamError> {
    let mut stream = provider.stream_chat(request, "test-model", StreamOptions::default());
    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        events.push(item?);
    }
    Ok(events)
}

// ---------------------------------------------------------------------------
// stream_chat：流式 SSE 成功解析
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_chat_parses_text_deltas_and_done() {
    let mut server = Server::new_async().await;
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n",
        "\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n",
        "\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n",
        "\n",
        "data: [DONE]\n",
    );
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body(sse_body)
        .create_async()
        .await;

    let provider = mock_provider(&server);
    let events = collect_stream_events(provider, simple_request("hi"))
        .await
        .unwrap();
    mock.assert_async().await;

    // 应有 2 个 TextDelta + 1 个 Done
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { content } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello world");

    assert!(
        events.iter().any(|e| matches!(
            e,
            StreamEvent::Done {
                finish_reason: FinishReason::Stop,
                ..
            }
        )),
        "应有 Stop finish_reason 的 Done 事件"
    );

    // usage 合并
    let done = events.iter().find_map(|e| match e {
        StreamEvent::Done { usage, .. } => Some(usage),
        _ => None,
    });
    let done = done.expect("应有 Done 事件");
    assert_eq!(done.total_tokens, 7);
}

#[tokio::test]
async fn stream_chat_parses_reasoning_deltas() {
    let mut server = Server::new_async().await;
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"思考中\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n",
    );
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body(sse_body)
        .create_async()
        .await;

    let provider = mock_provider(&server);
    let events = collect_stream_events(provider, simple_request("hi"))
        .await
        .unwrap();

    let reasoning: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ReasoningDelta { content } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, "思考中");
}

#[tokio::test]
async fn stream_chat_parses_tool_call_chunks() {
    let mut server = Server::new_async().await;
    // 工具调用分多个 chunk 增量到达：先 name/id，再 args_delta 分片
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"get_weather\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\\\"\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"BJ\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n",
    );
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body(sse_body)
        .create_async()
        .await;

    let provider = mock_provider(&server);
    let events = collect_stream_events(provider, simple_request("天气"))
        .await
        .unwrap();

    // 应有 ToolCallChunk 事件 + ToolCalls finish_reason
    let tool_chunks: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallChunk {
                index,
                id,
                name,
                args_delta,
            } => Some((index, id, name, args_delta)),
            _ => None,
        })
        .collect();
    assert!(!tool_chunks.is_empty(), "应有 ToolCallChunk 事件");
    // 第一个 chunk 应含 id 和 name
    assert_eq!(tool_chunks[0].0, &0);
    assert_eq!(tool_chunks[0].1.as_deref(), Some("call_1"));
    assert_eq!(tool_chunks[0].2.as_deref(), Some("get_weather"));

    assert!(
        events.iter().any(|e| matches!(
            e,
            StreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
                ..
            }
        )),
        "finish_reason 应为 ToolCalls"
    );
}

// ---------------------------------------------------------------------------
// chat：非流式成功解析
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_parses_content_and_usage() {
    let mut server = Server::new_async().await;
    let response_body = r#"{
        "choices": [{"message": {"content": "回复内容"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    }"#;
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body(response_body)
        .create_async()
        .await;

    let provider = mock_provider(&server);
    let response = provider
        .chat(simple_request("你好"), "test-model")
        .await
        .unwrap();

    assert_eq!(response.content.as_deref(), Some("回复内容"));
    assert_eq!(response.usage.total_tokens, 15);
    assert_eq!(response.finish_reason, FinishReason::Stop);
}

#[tokio::test]
async fn chat_parses_tool_calls() {
    let mut server = Server::new_async().await;
    let response_body = r#"{
        "choices": [{
            "message": {
                "tool_calls": [{
                    "id": "call_1",
                    "function": {"name": "search", "arguments": "{\"q\": \"rust\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    }"#;
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body(response_body)
        .create_async()
        .await;

    let provider = mock_provider(&server);
    let response = provider
        .chat(simple_request("搜索"), "test-model")
        .await
        .unwrap();

    let tool_calls = response.tool_calls.expect("应有 tool_calls");
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "call_1");
    assert_eq!(tool_calls[0].name, "search");
    assert_eq!(tool_calls[0].arguments, r#"{"q": "rust"}"#);
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
}

// ---------------------------------------------------------------------------
// HTTP 错误分类
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_chat_returns_auth_error_on_401() {
    let mut server = Server::new_async().await;
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(401)
        .with_body("unauthorized")
        .create_async()
        .await;

    let provider = mock_provider(&server);
    let result = collect_stream_events(provider, simple_request("hi")).await;
    assert!(
        matches!(result, Err(StreamError::AuthError(_))),
        "401 应映射为 AuthError"
    );
}

#[tokio::test]
async fn stream_chat_returns_rate_limit_on_429() {
    let mut server = Server::new_async().await;
    // 429 的 retry-after 从响应体文本解析（非 header）
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(429)
        .with_body("rate limited, retry-after-ms: 5000")
        .create_async()
        .await;

    let provider = mock_provider(&server);
    let result = collect_stream_events(provider, simple_request("hi")).await;
    match result {
        Err(StreamError::RateLimit { retry_after_ms, .. }) => {
            assert_eq!(retry_after_ms, Some(5000), "应从响应体解析 retry-after-ms");
        }
        other => panic!("429 应映射为 RateLimit，实际：{other:?}"),
    }
}

#[tokio::test]
async fn stream_chat_returns_context_overflow_on_400_with_marker() {
    let mut server = Server::new_async().await;
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(400)
        .with_body("context_length_exceeded: too many tokens")
        .create_async()
        .await;

    let provider = mock_provider(&server);
    let result = collect_stream_events(provider, simple_request("hi")).await;
    assert!(
        matches!(result, Err(StreamError::ContextOverflow)),
        "400 含 context_length_exceeded 应映射为 ContextOverflow"
    );
}

#[tokio::test]
async fn stream_chat_returns_api_error_on_500() {
    let mut server = Server::new_async().await;
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(500)
        .with_body("internal server error")
        .create_async()
        .await;

    let provider = mock_provider(&server);
    let result = collect_stream_events(provider, simple_request("hi")).await;
    assert!(
        matches!(result, Err(StreamError::ApiError(_))),
        "500 应映射为 ApiError"
    );
}

// ---------------------------------------------------------------------------
// chat 的错误分类（与 stream_chat 一致，走同一 send_request）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_returns_auth_error_on_403() {
    let mut server = Server::new_async().await;
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(403)
        .with_body("forbidden")
        .create_async()
        .await;

    let provider = mock_provider(&server);
    let result = provider.chat(simple_request("hi"), "test-model").await;
    assert!(
        matches!(result, Err(StreamError::AuthError(_))),
        "403 应映射为 AuthError"
    );
}

// ---------------------------------------------------------------------------
// from_parts：base_url 尾部斜杠归一化
// ---------------------------------------------------------------------------

#[tokio::test]
async fn from_parts_trims_trailing_slash_from_base_url() {
    // 带/不带尾斜杠应等价（请求都能命中 /v1/chat/completions）
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body(r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#)
        .create_async()
        .await;

    // 构造时带尾斜杠
    let provider = OpenAIProvider::from_parts(
        "test-key".to_string(),
        format!("{}/v1/", server.url()), // 注意尾斜杠
        reqwest::Client::new(),
    );
    let response = provider.chat(simple_request("hi"), "m").await.unwrap();
    assert_eq!(response.content.as_deref(), Some("ok"));
    mock.assert_async().await; // 证明请求命中了正确 URL
}
