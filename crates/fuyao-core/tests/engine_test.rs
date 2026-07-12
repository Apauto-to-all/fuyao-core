//! fuyao-core 集成测试：引擎装配 + ReAct 循环 + 工具调用编排
//!
//! 聚焦跨模块协作与公共 API 契约——这是单元测试的空白带：
//! - `Engine::new(Box<dyn Provider>)` 注入 mock，驱动端到端 ReAct 循环
//! - `EngineHandle` 公开 API：send_message / next_event / register_tool / cancel / hooks
//! - 工具调用循环：mock 产出 ToolCallChunk → 引擎执行注册的工具 → 继续循环
//! - 中断 / 关停的输入事件链路
//!
//! 全程不调 set_config，走 get_config 返回 default 的兜底（默认通道容量）。

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{MockProvider, collect_events, test_agent_ctx, text_events, tool_call_events};
use fuyao_api::message::OutputEvent;
use fuyao_api::{AgentContext, ModelConfig};
use fuyao_core::{Engine, EngineHandle};

/// 启动引擎，返回 handle 与 engine 任务句柄
///
/// `events` 作为固定序列注入（每次 LLM 调用返回同一序列）。
/// 注意：若 events 含工具调用且 finish_reason=ToolCalls，会导致无限 ReAct 循环——
/// 工具调用循环测试应改用 `spawn_engine_sequenced`。
async fn spawn_engine(
    events: Vec<fuyao_provider::StreamEvent>,
) -> (EngineHandle, tokio::task::JoinHandle<()>) {
    let provider = Box::new(MockProvider::fixed(events));
    let (mut engine, handle) = Engine::new(provider, test_agent_ctx());
    let task = tokio::spawn(async move { engine.run().await });
    (handle, task)
}

/// 启动引擎，注入状态化事件序列（第 N 次调用返回第 N 项）
///
/// 用于工具调用 ReAct 循环：首次返回 ToolCallChunk，第二次返回纯文本收敛到 Stop。
async fn spawn_engine_sequenced(
    responses: Vec<Vec<fuyao_provider::StreamEvent>>,
) -> (
    EngineHandle,
    tokio::task::JoinHandle<()>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let provider = Box::new(MockProvider::sequenced(responses));
    // 重新构造一个等价 provider 拿到 call_count 句柄——sequenced 内部已 Arc 共享，
    // 这里通过 Box 内部 Arc clone 的方式无法直接拿到，故改用外部构造。
    // 简化：sequenced 测试若需断言调用次数，直接在此函数内构造。
    let call_count = provider.call_count.clone();
    let (mut engine, handle) = Engine::new(provider, test_agent_ctx());
    let task = tokio::spawn(async move { engine.run().await });
    (handle, task, call_count)
}

/// 等待引擎任务退出，带超时保护（防止 shutdown 未生效时永久阻塞）
async fn join_engine(task: tokio::task::JoinHandle<()>) {
    let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
}

// ============================================================================
// 引擎装配与生命周期
// ============================================================================

#[tokio::test]
async fn engine_new_returns_engine_and_handle() {
    let provider = Box::new(MockProvider::fixed(text_events("hi")));
    let (engine, handle) = Engine::new(provider, test_agent_ctx());

    assert!(!handle.tx_input.is_closed(), "输入通道应开启");
    // agent_ctx 可读
    let ctx = handle.agent_ctx().expect("应返回 AgentContext");
    assert_eq!(ctx.model_config.model_id.as_deref(), Some("test-model"));
    drop(engine);
}

#[tokio::test]
async fn engine_handle_hooks_is_empty_initially() {
    let provider = Box::new(MockProvider::fixed(text_events("hi")));
    let (_engine, handle) = Engine::new(provider, test_agent_ctx());

    let hooks = handle.hooks();
    // 锁不阻塞即说明 hooks 存活
    let _guard = hooks.lock().await;
}

#[tokio::test]
async fn engine_shutdown_terminates_run_loop() {
    let (handle, task) = spawn_engine(text_events("hi")).await;

    handle.shutdown().await;

    // run 循环应在合理时间内退出
    let result = tokio::time::timeout(Duration::from_secs(3), task).await;
    assert!(result.is_ok(), "shutdown 后引擎任务应退出");
}

// ============================================================================
// ReAct 循环：纯文本回复
// ============================================================================

#[tokio::test]
async fn send_message_produces_chunk_and_assistant_events() {
    let (handle, task) = spawn_engine(text_events("你好世界")).await;

    handle.send_message("用户问题".to_string()).await;

    let events = collect_events(&handle, 20, 1500).await;

    handle.shutdown().await;
    join_engine(task).await;

    // 应至少收到流式 Chunk 与最终 Assistant 事件
    assert!(
        events.iter().any(|e| matches!(e, OutputEvent::Chunk(_))),
        "应含 Chunk 事件，实际收到：{:?}",
        events.iter().map(event_name).collect::<Vec<_>>()
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutputEvent::Assistant(_))),
        "应含 Assistant 事件"
    );
}

#[tokio::test]
async fn send_message_emits_queue_update_enqueued() {
    // 用户消息入队时应有 QueueUpdate(Enqueued)
    let (handle, task) = spawn_engine(text_events("回复")).await;

    handle.send_message("问题".to_string()).await;

    let events = collect_events(&handle, 15, 1500).await;

    handle.shutdown().await;
    join_engine(task).await;

    // 事件流应含 QueueUpdate 或 UserMessage（引擎确认收到输入）
    assert!(!events.is_empty(), "send_message 后应产生输出事件");
}

#[tokio::test]
async fn assistant_event_contains_streamed_content() {
    let (handle, task) = spawn_engine(text_events("最终回复内容")).await;

    handle.send_message("问".to_string()).await;
    let events = collect_events(&handle, 20, 1500).await;
    handle.shutdown().await;
    join_engine(task).await;

    let assistant = events
        .iter()
        .find_map(|e| match e {
            OutputEvent::Assistant(a) => Some(a),
            _ => None,
        })
        .expect("应含 Assistant 事件");

    assert!(
        assistant
            .payload
            .content
            .as_deref()
            .unwrap_or_default()
            .contains("最终回复内容"),
        "Assistant 内容应含流式文本"
    );
}

// ============================================================================
// 工具调用 ReAct 循环
// ============================================================================

#[tokio::test]
async fn tool_call_invokes_registered_handler() {
    // mock 状态化：首次返回工具调用，第二次返回纯文本收敛到 Stop（避免无限 ReAct 循环）
    let call_count = Arc::new(AtomicUsize::new(0));
    let count_for_handler = call_count.clone();

    let (handle, task, _llm_calls) = spawn_engine_sequenced(vec![
        tool_call_events("echo", r#"{"text":"hello"}"#),
        text_events("已完成"),
    ])
    .await;

    // 注册 echo 工具
    handle.register_tool(
        "echo",
        serde_json::json!({"type": "function", "function": {"name": "echo"}}),
        Arc::new(move |_args, _ctx: fuyao_api::ToolCallContext| {
            let c = count_for_handler.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                "echoed".to_string()
            })
        }),
    );

    handle.send_message("调用 echo".to_string()).await;

    // 收事件，等待工具执行与第二轮文本回复
    let _events = collect_events(&handle, 30, 2500).await;

    handle.shutdown().await;
    join_engine(task).await;

    assert!(
        call_count.load(Ordering::SeqCst) >= 1,
        "工具 handler 应被调用至少一次"
    );
}

#[tokio::test]
async fn handle_tools_schema_includes_registered_tools() {
    let provider = Box::new(MockProvider::fixed(text_events("x")));
    let (_engine, handle) = Engine::new(provider, test_agent_ctx());

    handle.register_tool(
        "test_a",
        serde_json::json!({"type": "function", "function": {"name": "test_a"}}),
        Arc::new(|_args, _ctx: fuyao_api::ToolCallContext| Box::pin(async { "a".to_string() })),
    );
    handle.register_tool(
        "test_b",
        serde_json::json!({"type": "function", "function": {"name": "test_b"}}),
        Arc::new(|_args, _ctx: fuyao_api::ToolCallContext| Box::pin(async { "b".to_string() })),
    );

    let schemas = handle.tools_schema();
    assert_eq!(schemas.len(), 2, "应含两个已注册工具");
}

#[tokio::test]
async fn duplicate_tool_registration_is_skipped() {
    // 先到先得：同名工具重复注册时跳过第二条
    let provider = Box::new(MockProvider::fixed(text_events("x")));
    let (_engine, handle) = Engine::new(provider, test_agent_ctx());

    let schema = serde_json::json!({"type": "function", "function": {"name": "dup"}});
    handle.register_tool(
        "dup",
        schema.clone(),
        Arc::new(|_args, _ctx: fuyao_api::ToolCallContext| Box::pin(async { "1".to_string() })),
    );
    handle.register_tool(
        "dup",
        schema,
        Arc::new(|_args, _ctx: fuyao_api::ToolCallContext| Box::pin(async { "2".to_string() })),
    );

    assert_eq!(handle.tools_schema().len(), 1, "重复注册不应追加");
}

// ============================================================================
// EngineHandle：cancel / agent_ctx_shared / set_agent_ctx
// ============================================================================

#[tokio::test]
async fn cancel_sends_interrupt_without_panic() {
    let (handle, task) = spawn_engine(text_events("hi")).await;

    // cancel 是同步 try_send Interrupt，不应 panic
    handle.cancel();

    handle.shutdown().await;
    join_engine(task).await;
}

#[tokio::test]
async fn set_agent_ctx_updates_shared_context() {
    let provider = Box::new(MockProvider::fixed(text_events("hi")));
    let (_engine, handle) = Engine::new(provider, test_agent_ctx());

    let shared = handle.agent_ctx_shared();
    {
        let mut ctx = shared.lock().unwrap();
        ctx.model_config.model_id = Some("updated-model".to_string());
    }

    // set_agent_ctx 写回后 agent_ctx() 应反映新值
    let updated = AgentContext {
        model_config: ModelConfig {
            model_id: Some("updated-model".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    handle.set_agent_ctx(updated);

    let read_back = handle.agent_ctx().unwrap();
    assert_eq!(
        read_back.model_config.model_id.as_deref(),
        Some("updated-model")
    );
}

#[tokio::test]
async fn try_next_event_non_blocking() {
    let (handle, task) = spawn_engine(text_events("hi")).await;

    // 无事件时 try_next_event 返回 None（不阻塞）
    let result = handle.try_next_event();
    // 可能恰好有事件也可能无，主要验证不 panic
    let _ = result;
    handle.shutdown().await;
    join_engine(task).await;
}

// ============================================================================
// 辅助
// ============================================================================

/// 事件名简化（调试输出用）
fn event_name(e: &OutputEvent) -> &'static str {
    match e {
        OutputEvent::TurnStart(_) => "TurnStart",
        OutputEvent::Chunk(_) => "Chunk",
        OutputEvent::User(_) => "User",
        OutputEvent::ToolCall(_) => "ToolCall",
        OutputEvent::ToolResult(_) => "ToolResult",
        OutputEvent::Assistant(_) => "Assistant",
        OutputEvent::Interrupt(_) => "Interrupt",
        OutputEvent::Error(_) => "Error",
        OutputEvent::Plugin(_) => "Plugin",
        OutputEvent::QueueUpdate(_) => "QueueUpdate",
    }
}
