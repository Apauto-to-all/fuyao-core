//! LLM 请求超时分工的集成测试（真实构造路径 + 原生 TCP server）
//!
//! 超时契约：流式对话不设请求总超时——长思考模型单次回复可达数十分钟，
//! 总死线会把健康的慢流拦腰掐断；非流式 `chat()`（标题生成等一次性短文本）
//! 设请求级总超时防止永久挂起。两用例共享同一份小超时全局配置
//! （`set_config` 的 OnceLock 只注入一次），并走 `OpenAIProvider::new`
//! 真实构造路径（注册表解析 + `resolve_http_parts` 构建 Client），
//! 保证被测的就是生产客户端构建代码。

use futures_util::StreamExt;
use fuyao_api::{
    AgentPaths, ApiProtocol, FuyaoConfig, LlmConfig, Provider as ProviderConfig, ProviderOptions,
    set_config,
};
use fuyao_provider::{
    ChatRequest, OpenAIProvider, Provider, StreamError, StreamEvent, StreamOptions,
    agent_paths_cache_key, clear_cache, register_provider,
};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 注入小超时全局配置（只注入一次）：`request_timeout_secs` 压到 2 秒，
/// 流式用例的总时长（约 4 秒）必须能越过它才能证明流式路径无总死线
fn init_config() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let config = FuyaoConfig {
            llm: LlmConfig {
                request_timeout_secs: 2,
                ..LlmConfig::default()
            },
            ..FuyaoConfig::default()
        };
        set_config(Arc::new(config));
    });
}

/// 注册带 API Key 与指定 base_url 的供应商，返回其唯一化 AgentPaths
/// （每用例独立 agent_id，注册表缓存互不干扰）
fn registered_paths(id: &str, base_url: String) -> AgentPaths {
    let paths = AgentPaths {
        agent_id: Some(format!("global/{id}")),
        workspace: None,
        ..AgentPaths::default()
    };
    let key = agent_paths_cache_key(&paths);
    let provider = ProviderConfig {
        name: id.to_string(),
        api_protocol: ApiProtocol::OpenaiCompletions,
        models: std::collections::HashMap::new(),
        options: ProviderOptions {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
        },
        api_key_env_vars: Vec::new(),
    };
    register_provider("vendor", provider, &key);
    paths
}

/// 读完请求（头 + body），避免客户端写侧阻塞
async fn drain_request(socket: &mut tokio::net::TcpStream) {
    let mut buf = vec![0u8; 65536];
    let _ = socket.read(&mut buf).await;
}

/// 流式对话不受 `request_timeout_secs` 约束：chunk 间隔与总时长都越过
/// 配置的 2 秒总超时，流仍完整产出全部增量并正常结束
#[tokio::test]
async fn stream_survives_beyond_request_timeout_secs() {
    init_config();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        drain_request(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                  Transfer-Encoding: chunked\r\n\r\n",
            )
            .await
            .unwrap();
        // 5 个 SSE chunk、间隔 800ms：总时长约 4 秒，远超 2 秒的请求总超时配置
        for i in 0..5 {
            let sse = format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"t{i}\"}}}}]}}\n\n");
            socket
                .write_all(format!("{:x}\r\n{sse}\r\n", sse.len()).as_bytes())
                .await
                .unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
        // 终结 chunk：流正常收尾
        socket.write_all(b"0\r\n\r\n").await.unwrap();
        socket.flush().await.unwrap();
    });

    let paths = registered_paths("timeout_stream", format!("http://{addr}"));
    let provider = OpenAIProvider::new("vendor", &paths).expect("Provider 构造失败");

    let mut stream = provider.stream_chat(
        ChatRequest::default(),
        "test-model",
        StreamOptions::default(),
    );
    let started = Instant::now();
    let mut deltas = 0usize;
    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamEvent::TextDelta { .. }) => deltas += 1,
            Ok(_) => {}
            Err(e) => panic!("总超时配置不应作用于流式路径：{e:?}"),
        }
    }
    assert_eq!(deltas, 5, "全部增量都应到达");
    assert!(
        started.elapsed() >= Duration::from_secs(3),
        "流应完整跑完约 4 秒（越过 2 秒总超时配置），实际 {:?}",
        started.elapsed()
    );
    clear_cache(&paths);
}

/// 非流式 `chat()` 受请求级总超时约束：服务器延迟响应超过配置的 2 秒时，
/// 以可重试的 [`StreamError::Timeout`] 失败，而不是等服务端
#[tokio::test]
async fn chat_fails_at_request_timeout_when_server_slow() {
    init_config();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        drain_request(&mut socket).await;
        // 迟到 4 秒的响应：请求级总超时应先行触发
        tokio::time::sleep(Duration::from_secs(4)).await;
        let _ = socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                  Content-Length: 2\r\n\r\n{}",
            )
            .await;
    });

    let paths = registered_paths("timeout_chat", format!("http://{addr}"));
    let provider = OpenAIProvider::new("vendor", &paths).expect("Provider 构造失败");

    let started = Instant::now();
    let result = provider
        .chat(
            ChatRequest::default(),
            "test-model",
            StreamOptions::default(),
        )
        .await;
    let elapsed = started.elapsed();
    match result {
        Err(StreamError::Timeout) => {}
        other => panic!("期望总超时触发的 Timeout，实际 {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(3),
        "超时应约 2 秒触发，实际 {elapsed:?}"
    );
    clear_cache(&paths);
}
