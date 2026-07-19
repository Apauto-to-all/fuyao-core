//! ReAct 循环单元测试
//!
//! 测试组织：MockProvider 驱动 + TestHarness 聚合共享依赖 + 临时 DB 隔离。
//! 覆盖 run_turn（7 个）与 run_session（1 个，pending 空闲解禁回归）。

use super::*;
use crate::engine::types::QueuedUserMessage;
use async_trait::async_trait;
use futures_util::stream;
use fuyao_api::MessageParams;
use fuyao_provider::{
    BoxStream, ChatResponse, FinishReason, StreamError, StreamEvent, StreamUsage,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;

/// 可控 Provider：测试侧持有 tx 精确控制事件节奏，事件之间有 await 挂起点
///
/// 与 [`MockProvider`] 的区别：MockProvider 用 `stream::iter` 一次性吐完所有事件，
/// 事件之间无 await 点，外层 select! 的中断分支无法胜出。ControllableProvider 的
/// `stream_chat` 返回从 channel 读取事件的流——每个事件之间 `recv().await` 是合法
/// 挂起点。测试侧发一个事件后**故意不发下一个**，流即挂起，此时发中断信号，
/// select! 的中断分支可靠胜出。
///
/// 支持多轮：用 [`with_batches`] 预置 N 个 rx（对应 ReAct N 轮 LLM 调用），
/// 每次 stream_chat 弹出一个 rx 消费，测试侧用对应索引的 tx 控制该轮事件。
///
/// 用于流式中断、工具执行中断等需要精确控制时序的测试。
struct ControllableProvider {
    pending: std::sync::Mutex<
        std::collections::VecDeque<
            tokio::sync::mpsc::UnboundedReceiver<Result<StreamEvent, StreamError>>,
        >,
    >,
}

impl ControllableProvider {
    /// 预置 N 轮事件流，返回 (provider, txs)——txs[i] 控制第 i 轮
    fn with_batches(
        n: usize,
    ) -> (
        Self,
        Vec<tokio::sync::mpsc::UnboundedSender<Result<StreamEvent, StreamError>>>,
    ) {
        let mut pending = std::collections::VecDeque::new();
        let mut txs = Vec::with_capacity(n);
        for _ in 0..n {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            pending.push_back(rx);
            txs.push(tx);
        }
        (
            Self {
                pending: std::sync::Mutex::new(pending),
            },
            txs,
        )
    }
}

#[async_trait]
impl Provider for ControllableProvider {
    fn stream_chat(
        &self,
        _request: fuyao_provider::ChatRequest,
        _model: &str,
        _options: fuyao_provider::StreamOptions,
    ) -> BoxStream<Result<StreamEvent, StreamError>> {
        // 弹出预置的 rx（第 i 次调用对应 txs[i]）
        let rx = self
            .pending
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("ControllableProvider 事件批次已用尽"));
        let s = async_stream::stream! {
            let mut rx = rx;
            while let Some(ev) = rx.recv().await {
                yield ev;
            }
        };
        Box::pin(s)
    }

    async fn chat(
        &self,
        _request: fuyao_provider::ChatRequest,
        _model: &str,
    ) -> Result<ChatResponse, StreamError> {
        Err(StreamError::ApiError("mock: chat 不支持".into()))
    }
}

/// Mock Provider：按预设序列依次返回不同的 StreamEvent 列表
///
/// 每次 stream_chat 调用消费 responses 里的一项（支持 ReAct 多轮）。用完返回空流。
struct MockProvider {
    responses: std::sync::Mutex<Vec<Vec<Result<StreamEvent, StreamError>>>>,
    call_count: AtomicUsize,
}

impl MockProvider {
    fn new(responses: Vec<Vec<Result<StreamEvent, StreamError>>>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
            call_count: AtomicUsize::new(0),
        }
    }

    /// 构造固定的文本回复流
    fn text_response(text: &str) -> Vec<Result<StreamEvent, StreamError>> {
        vec![
            Ok(StreamEvent::TextDelta {
                content: text.to_string(),
            }),
            Ok(StreamEvent::Done {
                usage: StreamUsage::default(),
                finish_reason: FinishReason::Stop,
            }),
        ]
    }

    /// 构造带用量统计的文本回复流（验证 usage 流到最终 AssistantMessage）
    fn text_response_with_usage(
        text: &str,
        usage: StreamUsage,
    ) -> Vec<Result<StreamEvent, StreamError>> {
        vec![
            Ok(StreamEvent::TextDelta {
                content: text.to_string(),
            }),
            Ok(StreamEvent::Done {
                usage,
                finish_reason: FinishReason::Stop,
            }),
        ]
    }

    /// 构造工具调用流
    fn tool_call_response(
        id: &str,
        name: &str,
        args: &str,
    ) -> Vec<Result<StreamEvent, StreamError>> {
        vec![
            Ok(StreamEvent::ToolCallChunk {
                index: 0,
                id: Some(id.to_string()),
                name: Some(name.to_string()),
                args_delta: Some(args.to_string()),
            }),
            Ok(StreamEvent::Done {
                usage: StreamUsage::default(),
                finish_reason: FinishReason::ToolCalls,
            }),
        ]
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn stream_chat(
        &self,
        _request: fuyao_provider::ChatRequest,
        _model: &str,
        _options: fuyao_provider::StreamOptions,
    ) -> BoxStream<Result<StreamEvent, StreamError>> {
        let mut responses = self.responses.lock().unwrap();
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let events = if responses.is_empty() {
            vec![]
        } else {
            responses.remove(0)
        };
        Box::pin(stream::iter(events))
    }

    async fn chat(
        &self,
        _request: fuyao_provider::ChatRequest,
        _model: &str,
    ) -> Result<ChatResponse, StreamError> {
        Err(StreamError::ApiError("mock: chat 不支持".into()))
    }
}

/// 构造临时 SessionStore（隔离的临时 DB 目录）
async fn temp_store() -> Arc<fuyao_session::SessionStore> {
    let dir = std::env::temp_dir()
        .join("fuyao_core_test")
        .join(uuid::Uuid::new_v4().to_string());
    let store = fuyao_session::SessionStore::new(dir.join("test.db"))
        .await
        .expect("构造 SessionStore 失败");
    Arc::new(store)
}

/// 构造一个注册了 echo 工具的 ToolRegistry
fn echo_registry() -> Arc<ToolRegistry> {
    let handler: fuyao_api::ToolFn = Arc::new(|args, _ctx| {
        let s = args.to_string();
        Box::pin(async move { format!("echo:{s}") })
    });
    let entry = crate::ToolEntry {
        definition: fuyao_api::ToolDefinition::new("echo", "回显参数"),
        handler,
    };
    Arc::new(ToolRegistry::builder().register(entry).build())
}

/// 构造空 SharedQueue
fn empty_queue() -> SharedQueue {
    Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()))
}

/// 构造空 SharedHooks（无拦截/观察钩子，管道纯透传）
fn empty_hooks() -> fuyao_hooks::SharedHooks {
    Arc::new(tokio::sync::Mutex::new(
        fuyao_hooks::HooksRegistry::default(),
    ))
}

/// 构造测试用 SessionCtx + session + rx_interrupt + 收事件的 rx
///
/// `tx_interrupt` 不发中断，但必须随 harness 存活以保持中断通道打开：
/// 若 tx 提前 drop，`rx_interrupt.recv()` 会立即就绪返回 None，在
/// `run_turn` 工具执行 select! 中随机抢占 exec_fut 分支，导致工具结果
/// 丢失、turn 提前结束（flaky 失败）。保留 tx 让 recv() 挂起等待，
/// 从而 stream/exec 分支稳定胜出。
#[allow(dead_code)]
struct TestHarness {
    ctx: SessionCtx,
    session: Session,
    rx_interrupt: Receiver<InterruptMessage>,
    #[allow(dead_code)]
    tx_interrupt: mpsc::Sender<InterruptMessage>,
    rx_event: mpsc::Receiver<OutputEvent>,
}

async fn make_harness(provider: Arc<dyn Provider>, tools: Arc<ToolRegistry>) -> TestHarness {
    let store = temp_store().await;
    let mut session = Session::new(None, Some("系统提示词".to_string()));
    store.create(&mut session).await.unwrap();
    let (tx_event, rx_event) = mpsc::channel(128);
    let (tx_interrupt, rx_interrupt) = mpsc::channel(8);
    let ctx = SessionCtx {
        store,
        provider,
        tools,
        hooks: empty_hooks(),
        agent_paths: fuyao_api::AgentPaths::default(),
        agent_config: fuyao_api::AgentConfig::default(),
        emitter: Emitter::new(tx_event, "test_session".to_string()),
        guide: empty_queue(),
        pending: empty_queue(),
        last_usage: Arc::new(tokio::sync::Mutex::new(None)),
        compression_state: Arc::new(std::sync::Mutex::new(
            fuyao_session::CompressionRuntimeState::default(),
        )),
        compression_config: fuyao_api::CompressionConfig::default(),
    };
    TestHarness {
        ctx,
        session,
        rx_interrupt,
        tx_interrupt,
        rx_event,
    }
}

/// 收集所有产出事件（直到通道暂时无数据）
async fn collect_events(rx: &mut mpsc::Receiver<OutputEvent>) -> Vec<OutputEvent> {
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    events
}

/// 提取事件的 session_id
fn event_session_id(event: &OutputEvent) -> Option<&str> {
    match event {
        OutputEvent::Chunk(m) => m.base.session_id.as_deref(),
        OutputEvent::User(m) => m.base.session_id.as_deref(),
        OutputEvent::ToolCall(m) => m.base.session_id.as_deref(),
        OutputEvent::ToolResult(m) => m.base.session_id.as_deref(),
        OutputEvent::Assistant(m) => m.base.session_id.as_deref(),
        OutputEvent::Interrupt(m) => m.base.session_id.as_deref(),
        OutputEvent::Error(m) => m.base.session_id.as_deref(),
        OutputEvent::Plugin(m) => m.base.session_id.as_deref(),
    }
}

/// 预置一条 user 消息进 session（模拟主循环 inject 后的状态）
fn preload_user(h: &mut TestHarness, content: &str) {
    h.session
        .messages
        .push(fuyao_api::Message::user(content.to_string()));
}

/// 无工具单轮：user → 流式回复 → AssistantMessage
#[tokio::test]
async fn single_turn_no_tools() {
    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response("你好")]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&mut h, "用户问题");

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        MessageParams::default(),
    )
    .await;

    let events = collect_events(&mut h.rx_event).await;
    let has_assistant = events.iter().any(
        |e| matches!(e, OutputEvent::Assistant(m) if m.payload.content.as_deref() == Some("你好")),
    );
    assert!(has_assistant, "应有 AssistantMessage 含「你好」");
    for ev in &events {
        assert_eq!(event_session_id(ev), Some("test_session"));
    }
    // user + assistant
    assert_eq!(h.session.messages.len(), 2);
    assert_eq!(h.session.messages[0].role, "user");
    assert_eq!(h.session.messages[1].role, "assistant");
}

/// 有工具循环：先工具调用 → 执行 echo → 再最终回复
#[tokio::test]
async fn react_loop_with_tool() {
    let provider = Arc::new(MockProvider::new(vec![
        MockProvider::tool_call_response("call_1", "echo", r#"{"msg":"hi"}"#),
        MockProvider::text_response("工具执行完毕"),
    ]));
    let mut h = make_harness(provider, echo_registry()).await;
    preload_user(&mut h, "调工具");

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        MessageParams::default(),
    )
    .await;

    let events = collect_events(&mut h.rx_event).await;
    let has_tool_result = events
        .iter()
        .any(|e| matches!(e, OutputEvent::ToolResult(m) if m.payload.tool_name == "echo"));
    assert!(has_tool_result, "应有 ToolResult 事件");
    assert!(
        h.session.messages.len() >= 3,
        "应含 user/assistant/tool 至少 3 条"
    );
    let last = h.session.messages.last().unwrap();
    assert_eq!(last.role, "assistant");
}

/// 工具结果进 messages（tool_call_id 回填）
#[tokio::test]
async fn tool_result_in_messages() {
    let provider = Arc::new(MockProvider::new(vec![
        MockProvider::tool_call_response("tc_42", "echo", r#"{"x":1}"#),
        MockProvider::text_response("完成"),
    ]));
    let mut h = make_harness(provider, echo_registry()).await;
    preload_user(&mut h, "test");

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        MessageParams::default(),
    )
    .await;

    let tool_msg = h
        .session
        .messages
        .iter()
        .find(|m| m.role == "tool")
        .expect("应有 tool 角色消息");
    assert_eq!(tool_msg.tool_call_id.as_deref(), Some("tc_42"));
    assert!(tool_msg.content.as_ref().unwrap().contains("echo"));
}

/// LLM 错误：失败直接发 Error 事件，不产出 AssistantMessage
#[tokio::test]
async fn llm_error_emits_error_event() {
    let provider = Arc::new(MockProvider::new(vec![vec![Err(StreamError::AuthError(
        "无效密钥".into(),
    ))]]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&mut h, "test");

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        MessageParams::default(),
    )
    .await;

    let events = collect_events(&mut h.rx_event).await;
    let has_error = events
        .iter()
        .any(|e| matches!(e, OutputEvent::Error(m) if !m.payload.recoverable));
    assert!(has_error, "应有 recoverable=false 的 Error 事件");
    let has_assistant = events
        .iter()
        .any(|e| matches!(e, OutputEvent::Assistant(_)));
    assert!(!has_assistant, "错误时不应产出 AssistantMessage");
}

/// 工具完成后一次性消费 guide 全部消息
#[tokio::test]
async fn guide_all_consumed_on_tool_complete() {
    let provider = Arc::new(MockProvider::new(vec![
        MockProvider::tool_call_response("c1", "echo", r#"{}"#),
        // 第二轮（工具结果 + guide 消息都带上后）最终回复
        MockProvider::text_response("done"),
    ]));
    let mut h = make_harness(provider, echo_registry()).await;
    preload_user(&mut h, "原始问题");
    // 工具执行期间用户补充 2 条 guide 消息（模拟入队）
    h.ctx.guide.lock().unwrap().push_back(QueuedUserMessage {
        content: "补充1".into(),
        params: MessageParams::default(),
    });
    h.ctx.guide.lock().unwrap().push_back(QueuedUserMessage {
        content: "补充2".into(),
        params: MessageParams::default(),
    });

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        MessageParams::default(),
    )
    .await;

    // session.messages 应含：原始user + assistant(tool_calls) + tool + 补充1 + 补充2 + assistant(最终)
    let user_msgs: Vec<_> = h
        .session
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.clone().unwrap_or_default())
        .collect();
    assert!(user_msgs.contains(&"原始问题".to_string()));
    assert!(
        user_msgs.contains(&"补充1".to_string()),
        "guide 第一条应注入"
    );
    assert!(
        user_msgs.contains(&"补充2".to_string()),
        "guide 第二条应注入"
    );
    // guide 应被清空
    assert!(h.ctx.guide.lock().unwrap().is_empty(), "guide 应被消费空");
}

/// 最终回复后：pending 先倒 guide，再 guide 全消费（pending 优先于 guide）
#[tokio::test]
async fn pending_before_guide_on_final_reply() {
    // 第一轮直接最终回复（无工具），触发消费时机②
    let provider = Arc::new(MockProvider::new(vec![
        MockProvider::text_response("回复1"),
        // 注入 pending+guide 消息后再调一轮，最终回复
        MockProvider::text_response("回复2"),
    ]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&mut h, "第一条");
    // 预置 pending 1 条 + guide 1 条（最终回复后应都被消费）
    h.ctx.pending.lock().unwrap().push_back(QueuedUserMessage {
        content: "排队消息".into(),
        params: MessageParams::default(),
    });
    h.ctx.guide.lock().unwrap().push_back(QueuedUserMessage {
        content: "引导消息".into(),
        params: MessageParams::default(),
    });

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        MessageParams::default(),
    )
    .await;

    // 两条都应进 messages
    let user_msgs: Vec<_> = h
        .session
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.clone().unwrap_or_default())
        .collect();
    assert!(
        user_msgs.contains(&"排队消息".to_string()),
        "pending 应被消费"
    );
    assert!(
        user_msgs.contains(&"引导消息".to_string()),
        "guide 应被消费"
    );
    // 两个队列都空
    assert!(h.ctx.pending.lock().unwrap().is_empty());
    assert!(h.ctx.guide.lock().unwrap().is_empty());
}

/// guide + pending 都空，最终回复后 turn 结束（不追加额外消息）
#[tokio::test]
async fn both_empty_turn_ends() {
    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response("回复")]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&mut h, "问题");

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        MessageParams::default(),
    )
    .await;

    // user + assistant，没有多余消息
    assert_eq!(h.session.messages.len(), 2);
}

/// 回归：task 空闲时只发 pending 也能触发新 turn（pending 不应死信）
///
/// 覆盖 main 循环的 drain_pending 补丁：task 空闲 = 无活跃 ReAct 链，
/// pending 的"等链结束"解禁条件已满足，应立即解禁进 guide 触发新 turn。
/// 修复前：main 循环只 consume guide，task 空闲时 pending 永远进不了 turn（死信）。
#[tokio::test]
async fn pending_consumed_when_task_idle() {
    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response(
        "已收到",
    )]));

    let store = temp_store().await;
    let mut session = Session::new(None, Some("系统提示词".to_string()));
    store.create(&mut session).await.unwrap();

    let guide = empty_queue();
    let pending = empty_queue();
    // 入站通道（User 消息经此送进 session task 过管道入队）
    let (tx_inbound, rx_inbound) = mpsc::channel::<fuyao_api::InboundUser>(16);
    // tx 必须随测试存活以保持中断通道打开（rx_interrupt.recv() 不提前返回 None）
    let _tx_interrupt = mpsc::channel::<InterruptMessage>(8).0;
    let (rx_interrupt_tx, rx_interrupt) = mpsc::channel::<InterruptMessage>(8);
    std::mem::forget(rx_interrupt_tx);
    // Plugin 通道（保持打开，避免 rx_plugin.recv() 提前返回 None）
    let (_tx_plugin, rx_plugin) = mpsc::channel::<fuyao_api::message::input::PluginMessage>(16);
    let (tx_event, mut rx_event) = mpsc::channel(128);

    // 启动 session 执行流（两队列都空，task 进入 select! 等待）
    let task = tokio::spawn(run_session(
        "test_session".to_string(),
        Arc::clone(&guide),
        Arc::clone(&pending),
        rx_inbound,
        rx_interrupt,
        rx_plugin,
        session,
        Arc::clone(&store),
        provider,
        Arc::new(ToolRegistry::builder().build()),
        empty_hooks(),
        fuyao_api::AgentPaths::default(),
        fuyao_api::AgentConfig::default(),
        tx_event,
    ));

    // 模拟 send：经入站通道发一条 Pending 消息（过管道入 pending 队列）
    // task 空闲（无活跃 ReAct 链），pending 的解禁条件已满足
    tx_inbound
        .send(fuyao_api::InboundUser {
            content: "排队消息".into(),
            mode: fuyao_api::UserMessageMode::Pending,
            params: MessageParams::default(),
        })
        .await
        .unwrap();

    // 期待 pending 被解禁 → turn 跑完 → AssistantMessage（2 秒超时防止 bug 时挂死）
    let mut got_assistant = false;
    let timed_out = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(ev) = rx_event.recv().await {
            if matches!(ev, OutputEvent::Assistant(m) if m.payload.content.as_deref() == Some("已收到"))
            {
                got_assistant = true;
                break;
            }
        }
    })
    .await
    .is_err();

    task.abort();

    assert!(
        !timed_out,
        "2 秒内未收到 Assistant 事件，pending 死信（bug 未修复）"
    );
    assert!(got_assistant, "应收到含「已收到」的 AssistantMessage");
    assert!(pending.lock().unwrap().is_empty(), "pending 应被消费空");
    assert!(guide.lock().unwrap().is_empty(), "guide 应保持空");
}

/// Plugin 消息路由：经 tx_plugin 通道发 InputEvent::Plugin 携带的 PluginMessage →
/// 从 rx_event 流出 OutputEvent::Plugin（session_id 标签正确）。
///
/// 验证阶段 2 新链路：
/// - InputEvent::Plugin 不再在 Engine 层直发 OutputEvent::Plugin
/// - 改为送进 session 的 tx_plugin 通道，由 session task 过 dispatch 管道
/// - 经 Emitter 自动盖 session_id 标签
#[tokio::test]
async fn plugin_message_routes_through_dispatch() {
    use fuyao_api::message::input::{PluginEventSource, PluginMessage, PluginPayload};

    // 不会被调用（Plugin 消息不触发 ReAct），随便给个空响应占位
    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response("ok")]));

    let store = temp_store().await;
    let mut session = Session::new(None, Some("系统提示词".to_string()));
    store.create(&mut session).await.unwrap();

    let guide = empty_queue();
    let pending = empty_queue();
    let (_tx_inbound, rx_inbound) = mpsc::channel::<fuyao_api::InboundUser>(16);
    let _tx_interrupt = mpsc::channel::<InterruptMessage>(8).0;
    let (rx_interrupt_tx, rx_interrupt) = mpsc::channel::<InterruptMessage>(8);
    std::mem::forget(rx_interrupt_tx);
    // tx_plugin 需要保留以发送消息
    let (tx_plugin, rx_plugin) = mpsc::channel::<fuyao_api::message::input::PluginMessage>(16);
    let (tx_event, mut rx_event) = mpsc::channel(128);

    let task = tokio::spawn(run_session(
        "plugin_session".to_string(),
        Arc::clone(&guide),
        Arc::clone(&pending),
        rx_inbound,
        rx_interrupt,
        rx_plugin,
        session,
        Arc::clone(&store),
        provider,
        Arc::new(ToolRegistry::builder().build()),
        empty_hooks(),
        fuyao_api::AgentPaths::default(),
        fuyao_api::AgentConfig::default(),
        tx_event,
    ));

    // 模拟 Engine::send 的 Plugin 分支：发一条 Plugin 消息到 tx_plugin 通道
    tx_plugin
        .send(PluginMessage {
            base: fuyao_api::message::EventBase::default(),
            payload: PluginPayload {
                source: PluginEventSource {
                    name: "loop_guard".into(),
                },
                event_type: "loop_warn".into(),
                data: None,
                error: None,
                message: Some("检测到循环".into()),
            },
        })
        .await
        .unwrap();

    // 期待从 rx_event 收到 OutputEvent::Plugin（带 session_id 标签）
    let mut got_plugin = false;
    let timed_out = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(ev) = rx_event.recv().await {
            if let OutputEvent::Plugin(m) = ev {
                assert_eq!(m.payload.source.name, "loop_guard");
                assert_eq!(m.payload.event_type, "loop_warn");
                assert_eq!(m.payload.message.as_deref(), Some("检测到循环"));
                // 经 Emitter 自动盖 session_id 标签
                assert_eq!(
                    m.base.session_id.as_deref(),
                    Some("plugin_session"),
                    "Plugin 事件应盖 session_id 标签"
                );
                got_plugin = true;
                break;
            }
        }
    })
    .await
    .is_err();

    task.abort();

    assert!(
        !timed_out,
        "2 秒内未收到 Plugin 事件，tx_plugin → dispatch 管道路由未通"
    );
    assert!(got_plugin, "应收到 OutputEvent::Plugin");
}

/// 中断-流式期间：流式中途发 Interrupt → 产出 Interrupt 事件 + 部分 AssistantMessage（finish_reason=interrupted）
///
/// 时序：ControllableProvider 吐一个 TextDelta 后故意不发下一个（流挂起），
/// 此时发中断信号，run_turn 的 select!（turn.rs:64）中断分支胜出。
/// handle_interrupt 读 TurnState 已累积的"你好"发部分 AssistantMessage。
#[tokio::test]
async fn interrupt_during_streaming() {
    use fuyao_api::message::input::InterruptSource;
    use fuyao_api::message::input::{InterruptMessage, InterruptPayload};

    // 准备 1 轮事件流（中断发生在首轮流式期间）
    let (provider, txs) = ControllableProvider::with_batches(1);
    let provider: Arc<dyn Provider> = Arc::new(provider);
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&mut h, "test");

    let tx_interrupt = h.tx_interrupt.clone();

    // 喂一个 TextDelta（run_turn 尚未启动，先入队，流启动后即消费）
    txs[0]
        .send(Ok(StreamEvent::TextDelta {
            content: "你好".to_string(),
        }))
        .unwrap();

    // run_turn 与"发中断"并发：run_turn 先消费 TextDelta，然后挂起在第二个事件上；
    // yield_now 让出调度让 run_turn 进入挂起态，再发中断。
    let turn_fut = turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        MessageParams::default(),
    );
    tokio::pin!(turn_fut);
    let interrupter = async {
        // 等 run_turn 跑起来并挂起在流的第二个事件上
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        tx_interrupt
            .send(InterruptMessage {
                base: fuyao_api::message::EventBase::default(),
                payload: InterruptPayload {
                    reason: "用户取消".into(),
                    source: InterruptSource::User,
                },
            })
            .await
            .unwrap();
    };
    tokio::select! {
        _ = &mut turn_fut => {}
        _ = interrupter => {
            // 中断信号已发，等 turn_fut 自己因 select! 中断分支结束
            tokio::time::timeout(std::time::Duration::from_secs(2), turn_fut)
                .await
                .expect("run_turn 应在中断后结束");
        }
    }

    let events = collect_events(&mut h.rx_event).await;
    // 断言 Interrupt 通知事件
    let has_interrupt = events
        .iter()
        .any(|e| matches!(e, OutputEvent::Interrupt(_)));
    assert!(has_interrupt, "应有 Interrupt 事件");
    // 断言部分 AssistantMessage（finish_reason=interrupted，content 含已吐出的"你好"）
    let partial = events.iter().find_map(|e| match e {
        OutputEvent::Assistant(m) if m.payload.finish_reason.as_deref() == Some("interrupted") => {
            Some(m)
        }
        _ => None,
    });
    let partial = partial.expect("应有 finish_reason=interrupted 的 AssistantMessage");
    assert_eq!(
        partial.payload.content.as_deref(),
        Some("你好"),
        "部分 AssistantMessage 应含已累积的文本"
    );
}

/// 中断-工具执行期间：工具 handler 阻塞时发 Interrupt → 产出 Interrupt + 中断式 ToolResult
///
/// 时序：MockProvider 吐工具调用 → run_turn 进入工具执行 select!（turn.rs:172），
/// 阻塞式工具 handler 卡住 → 发中断信号 → exec_fut 被 drop，统一补发中断式 ToolResult。
#[tokio::test]
async fn interrupt_during_tool_execution() {
    use fuyao_api::message::input::InterruptSource;
    use fuyao_api::message::input::{InterruptMessage, InterruptPayload};

    let provider = Arc::new(MockProvider::new(vec![MockProvider::tool_call_response(
        "tc_block",
        "blocking_tool",
        r#"{}"#,
    )]));

    // 注册阻塞工具：handler 等一个永不到来的信号，确保中断前不会完成
    let blocking_handler: fuyao_api::ToolFn = Arc::new(|_args, _ctx| {
        Box::pin(async {
            // 永不完成：sleep 30 秒，足够测试发中断
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            "unreachable".to_string()
        })
    });
    let tools = ToolRegistry::builder()
        .register(crate::ToolEntry {
            definition: fuyao_api::ToolDefinition::new("blocking_tool", "阻塞测试工具"),
            handler: blocking_handler,
        })
        .build();

    let mut h = make_harness(provider, Arc::new(tools)).await;
    preload_user(&mut h, "调工具");

    let tx_interrupt = h.tx_interrupt.clone();

    let turn_fut = turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        MessageParams::default(),
    );
    tokio::pin!(turn_fut);
    let interrupter = async {
        // 等 run_turn 跑完流式（工具调用）并进入工具执行阻塞
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        tx_interrupt
            .send(InterruptMessage {
                base: fuyao_api::message::EventBase::default(),
                payload: InterruptPayload {
                    reason: "用户取消".into(),
                    source: InterruptSource::User,
                },
            })
            .await
            .unwrap();
    };
    tokio::select! {
        _ = &mut turn_fut => {}
        _ = interrupter => {
            tokio::time::timeout(std::time::Duration::from_secs(2), turn_fut)
                .await
                .expect("run_turn 应在工具中断后结束");
        }
    }

    let events = collect_events(&mut h.rx_event).await;
    let has_interrupt = events
        .iter()
        .any(|e| matches!(e, OutputEvent::Interrupt(_)));
    assert!(has_interrupt, "应有 Interrupt 事件");
    // 中断式 ToolResult：统一补发，content 格式 [{source:?}][{reason}]
    let tool_result = events.iter().find_map(|e| match e {
        OutputEvent::ToolResult(m) if m.payload.tool_name == "blocking_tool" => Some(m),
        _ => None,
    });
    let tool_result = tool_result.expect("应有 blocking_tool 的中断式 ToolResult");
    assert_eq!(tool_result.payload.tool_call_id, "tc_block");
    assert!(
        tool_result.payload.content.contains("User"),
        "中断式 ToolResult content 应含中断来源: {}",
        tool_result.payload.content
    );
    assert!(
        tool_result.payload.content.contains("用户取消"),
        "中断式 ToolResult content 应含中断原因: {}",
        tool_result.payload.content
    );
}

/// 落库验证：跑完一轮含工具调用的 ReAct，落库后重新加载，messages 完整持久化
///
/// 验证 persist() → store.update() 确实把完整 messages 序列化进 DB，
/// 不只更新 message_count。从 store.get() 重新加载验证完整性。
#[tokio::test]
async fn messages_persisted_to_db() {
    let provider = Arc::new(MockProvider::new(vec![
        MockProvider::tool_call_response("tc_99", "echo", r#"{"msg":"hi"}"#),
        MockProvider::text_response("完成"),
    ]));
    let mut h = make_harness(provider, echo_registry()).await;
    preload_user(&mut h, "调工具");
    let session_id = h.session.id.clone();

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        MessageParams::default(),
    )
    .await;

    // 内存中的 messages（run_turn 后的完整状态）
    let expected_count = h.session.messages.len();
    assert!(
        expected_count >= 4,
        "应含 user/assistant(tool_calls)/tool/assistant(最终) 至少 4 条"
    );

    // 从 DB 重新加载，验证持久化的完整性
    let reloaded = h
        .ctx
        .store
        .get(&session_id)
        .await
        .expect("get 不应失败")
        .expect("session 应已落库");
    assert_eq!(
        reloaded.messages.len(),
        expected_count,
        "重新加载的 messages 数应与内存一致（持久化完整）"
    );
    assert_eq!(
        reloaded.message_count as usize, expected_count,
        "message_count 应与实际 messages 数一致"
    );

    // 验证关键消息类型完整保留
    let roles: Vec<_> = reloaded.messages.iter().map(|m| m.role.as_str()).collect();
    assert!(roles.contains(&"user"), "应含 user 消息");
    assert!(roles.contains(&"tool"), "应含 tool 消息");
    assert!(
        roles.iter().filter(|r| **r == "assistant").count() >= 2,
        "应含至少 2 条 assistant"
    );

    // 验证 tool 消息的 tool_call_id 回填正确
    let tool_msg = reloaded
        .messages
        .iter()
        .find(|m| m.role == "tool")
        .expect("应有 tool 消息");
    assert_eq!(
        tool_msg.tool_call_id.as_deref(),
        Some("tc_99"),
        "tool 消息应回填正确的 tool_call_id"
    );
}

/// 用量统计流通：模型在 Done 事件给出的 usage，应原样出现在最终 AssistantMessage 的 token 字段
///
/// 验证修复重构漏搬：usage 不再被硬编码为 0，而是从 decoder.usage() 流经 StreamResult
/// 到最终 AssistantMessage 的 5 个 token 字段（completion/prompt/total/reasoning/cached）。
#[tokio::test]
async fn usage_flows_to_final_assistant_message() {
    let usage = StreamUsage {
        prompt_tokens: 120,
        completion_tokens: 80,
        total_tokens: 200,
        completion_reasoning_tokens: Some(30),
        prompt_cached_tokens: Some(40),
    };
    let provider = Arc::new(MockProvider::new(vec![
        MockProvider::text_response_with_usage("回复内容", usage),
    ]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&mut h, "提问");

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        MessageParams::default(),
    )
    .await;

    let events = collect_events(&mut h.rx_event).await;
    let assistant = events
        .iter()
        .find_map(|e| match e {
            OutputEvent::Assistant(m) if m.payload.finish_reason.as_deref() == Some("stop") => {
                Some(m)
            }
            _ => None,
        })
        .expect("应有 finish_reason=stop 的最终 AssistantMessage");

    let p = &assistant.payload;
    assert_eq!(
        p.completion_tokens, 80,
        "completion_tokens 应来自模型 usage"
    );
    assert_eq!(p.prompt_tokens, 120, "prompt_tokens 应来自模型 usage");
    assert_eq!(p.total_tokens, 200, "total_tokens 应来自模型 usage");
    assert_eq!(
        p.reasoning_tokens, 30,
        "reasoning_tokens 应来自 completion_reasoning_tokens"
    );
    assert_eq!(
        p.cached_tokens, 40,
        "cached_tokens 应来自 prompt_cached_tokens"
    );
}
