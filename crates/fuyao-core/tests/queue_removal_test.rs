//! 双队列消息删除：client_message_id 全链路透传与按标识删除
//!
//! 端到端闭环（经公开 API：send / remove_queued_message / stop_session）：
//! - turn 流式挂起期间入队（guide / pending 各有目标与保留条目）→ 按标识分别
//!   删除两队列中的条目
//! - 无匹配标识返回 0
//! - 被删消息不回显、不落库；保留消息正常消费且回显事件携带原 client_message_id
//!   （入队 → 队列流转 → 消费回显的透传闭环）
//! - DB 中 user 消息集合与回显一致

mod common;

use std::time::Duration;

use common::{make_store, temp_agent_paths};
use fuyao_api::message::input::{UserMessage, UserMessageMode, UserMessageSource, UserPayload};
use fuyao_api::message::{EventBase, InputEvent, OutputEvent};
use fuyao_api::{AgentConfig, EngineParams, MessageRole, ModelConfig, SessionParams};
use fuyao_core::{Engine, PluginHost};

/// 吐一个 TextDelta 后永久挂起的 Provider
///
/// turn 稳定停在流式阶段：流式 select! 即时入队新消息，而队列的消费要等
/// 流结束——入队与删除之间形成不受竞争影响的稳定窗口
struct HangingMidStreamProvider;

#[async_trait::async_trait]
impl fuyao_provider::Provider for HangingMidStreamProvider {
    fn stream_chat(
        &self,
        _request: fuyao_provider::ChatRequest,
        _model: &str,
        _options: fuyao_provider::StreamOptions,
    ) -> fuyao_provider::BoxStream<Result<fuyao_provider::StreamEvent, fuyao_provider::StreamError>>
    {
        let s = async_stream::stream! {
            yield Ok(fuyao_provider::StreamEvent::TextDelta {
                content: "部分输出".to_string(),
            });
            // 永久挂起：不发 Done，流不结束（turn 卡在流式阶段等停止信号）
            futures_util::future::pending::<()>().await;
        };
        Box::pin(s)
    }

    async fn chat(
        &self,
        _request: fuyao_provider::ChatRequest,
        _model: &str,
        _options: fuyao_provider::StreamOptions,
    ) -> Result<fuyao_provider::ChatResponse, fuyao_provider::StreamError> {
        Err(fuyao_provider::StreamError::ApiError {
            status: None,
            message: "mock: chat 不支持".into(),
        })
    }
}

/// 构造带客户端消息标识的用户输入事件
fn message_with_id(content: &str, client_message_id: &str, mode: UserMessageMode) -> InputEvent {
    InputEvent::User(UserMessage {
        base: EventBase::default(),
        payload: UserPayload {
            content: content.to_string(),
            images: vec![],
            mode,
            source: UserMessageSource::User,
            client_message_id: Some(client_message_id.to_string()),
        },
    })
}

/// 轮询重试删除：send 返回只代表消息进入 inbound 通道，session task 异步入队
/// 存在微小延迟——重试直到删除成功或超时（超时返回 0，交由调用方断言失败）
async fn remove_until_deleted(engine: &Engine, session_id: &str, client_message_id: &str) -> usize {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let target_id = session_id.to_string();
    loop {
        let removed = engine
            .remove_queued_message(&target_id, client_message_id)
            .await
            .expect("删除调用失败");
        if removed > 0 || tokio::time::Instant::now() >= deadline {
            return removed;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 等待并收集一条指定标识的 User 回显事件（带超时）
///
/// 返回回显事件携带的 (content, client_message_id)，超时 panic
async fn await_user_echo(
    rx_event: &mut tokio::sync::mpsc::UnboundedReceiver<OutputEvent>,
    client_message_id: &str,
) -> (String, Option<String>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ev = tokio::time::timeout_at(deadline, rx_event.recv())
            .await
            .expect("等待 User 回显超时")
            .expect("session 事件通道不应提前关闭");
        if let OutputEvent::User(m) = ev
            && m.payload.client_message_id.as_deref() == Some(client_message_id)
        {
            return (
                m.payload.content.clone(),
                m.payload.client_message_id.clone(),
            );
        }
    }
}

/// 全链路：入队带标识 → 按标识删除（guide / pending 两队列）→ 后续消费的
/// 回显与落库均不含被删消息，保留消息回显携带原标识
#[tokio::test]
async fn remove_queued_message_end_to_end_flow() {
    let (agent_paths, _home) = temp_agent_paths();
    let store = make_store(&agent_paths).await;

    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        fuyao_provider::ProviderRegistry::with_instance(
            "test",
            std::sync::Arc::new(HangingMidStreamProvider),
        ),
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
        std::sync::Arc::clone(&store),
    )
    .await;

    let (session_id, mut rx_event) = engine
        .create_session(SessionParams {
            agent_config: AgentConfig {
                definition: "default".to_string(),
            },
            model_config: ModelConfig {
                model_id: "test/model".to_string(),
                thinking_type: None,
                reasoning_effort: None,
            },
        })
        .await
        .expect("创建 session 失败");

    // 第一条消息触发 turn，等 Chunk 事件确认 turn 已挂在流式阶段
    engine
        .send(
            &session_id,
            message_with_id("首轮消息", "cmid-a", UserMessageMode::Guide),
        )
        .await
        .expect("发送失败");
    let echo_a = await_user_echo(&mut rx_event, "cmid-a").await;
    assert_eq!(echo_a.0, "首轮消息");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ev = tokio::time::timeout_at(deadline, rx_event.recv())
            .await
            .expect("等待进入流式阶段超时")
            .expect("session 事件通道不应提前关闭");
        if matches!(ev, OutputEvent::Chunk(_)) {
            break;
        }
    }

    // turn 挂起期间入队：guide 与 pending 各一条待删 + pending 一条保留
    engine
        .send(
            &session_id,
            message_with_id("guide 待删", "cmid-b", UserMessageMode::Guide),
        )
        .await
        .expect("发送失败");
    engine
        .send(
            &session_id,
            message_with_id("pending 待删", "cmid-c", UserMessageMode::Pending),
        )
        .await
        .expect("发送失败");
    engine
        .send(
            &session_id,
            message_with_id("pending 保留", "cmid-e", UserMessageMode::Pending),
        )
        .await
        .expect("发送失败");

    // 按标识删除：guide 与 pending 各删 1 条；无匹配标识返回 0；重复删除返回 0
    assert_eq!(
        remove_until_deleted(&engine, &session_id, "cmid-b").await,
        1,
        "guide 队列应删 1 条"
    );
    assert_eq!(
        remove_until_deleted(&engine, &session_id, "cmid-c").await,
        1,
        "pending 队列应删 1 条"
    );
    assert_eq!(
        engine
            .remove_queued_message(&session_id, "cmid-absent")
            .await
            .expect("无匹配删除应成功"),
        0
    );
    assert_eq!(
        engine
            .remove_queued_message(&session_id, "cmid-b")
            .await
            .expect("重复删除应成功"),
        0,
        "已删净的标识再次删除应返回 0"
    );

    // 屏障停止：首轮 turn 终止且中断收尾落库完毕；队列剩余（pending 保留条目）原样保留
    engine
        .stop_session(&session_id, "测试停止")
        .await
        .expect("停止失败");

    // 新消息（pending 模式）恢复消费：主循环把 pending 剩余（保留条目）与新消息
    // 一起倒进 guide 注入消费——被删的两条不复活
    engine
        .send(
            &session_id,
            message_with_id("后续消息", "cmid-d", UserMessageMode::Pending),
        )
        .await
        .expect("发送失败");

    // 保留条目与新消息的回显携带原标识（透传闭环）
    let echo_e = await_user_echo(&mut rx_event, "cmid-e").await;
    assert_eq!(echo_e.0, "pending 保留");
    assert_eq!(echo_e.1.as_deref(), Some("cmid-e"));
    let echo_d = await_user_echo(&mut rx_event, "cmid-d").await;
    assert_eq!(echo_d.0, "后续消息");
    assert_eq!(echo_d.1.as_deref(), Some("cmid-d"));

    // 收尾：停掉第二次挂起的 turn，引擎优雅退出，DB 进入终态
    engine
        .stop_session(&session_id, "测试收尾")
        .await
        .expect("收尾停止失败");
    engine.shutdown().await;

    // DB 断言：user 消息恰为回显过的三条（首轮 + pending 保留 + 后续），
    // 被删的两条未落库
    let history = store
        .load_full_history(&session_id)
        .await
        .expect("读取历史失败");
    let user_contents: Vec<String> = history
        .iter()
        .filter(|m| matches!(m.role, MessageRole::User))
        .map(|m| m.content.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        user_contents,
        vec![
            "首轮消息".to_string(),
            "pending 保留".to_string(),
            "后续消息".to_string(),
        ],
        "DB 中 user 消息应为回显过的三条，被删消息不得落库"
    );
}
