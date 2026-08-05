//! ReAct 循环单元测试
//!
//! 测试组织：MockProvider 驱动 + TestHarness 聚合共享依赖 + 临时 DB 隔离。
//! 覆盖 run_turn（7 个）与 run_session（1 个，pending 空闲解禁回归）。

use super::*;
use async_trait::async_trait;
use futures_util::stream;
use fuyao_api::MessageRole;
use fuyao_api::message::EventBase;
use fuyao_api::message::input::{UserMessageMode, UserMessageSource};
use fuyao_api::message::output::{
    InterruptMessage as OutputInterruptMessage, PluginMessage as OutputPluginMessage,
    UserMessage as OutputUserMessage, UserPayload as OutputUserPayload,
};
use fuyao_provider::{
    BoxStream, ChatResponse, FinishReason, Provider, StreamError, StreamEvent, StreamUsage,
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
        _options: fuyao_provider::StreamOptions,
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

    /// 构造带用量统计的工具调用流（验证工具调用轮的 usage 也累积进 session 总计）
    fn tool_call_response_with_usage(
        id: &str,
        name: &str,
        args: &str,
        usage: StreamUsage,
    ) -> Vec<Result<StreamEvent, StreamError>> {
        vec![
            Ok(StreamEvent::ToolCallChunk {
                index: 0,
                id: Some(id.to_string()),
                name: Some(name.to_string()),
                args_delta: Some(args.to_string()),
            }),
            Ok(StreamEvent::Done {
                usage,
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
        _options: fuyao_provider::StreamOptions,
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
    let handler: fuyao_api::ToolFn = Arc::new(|args, _ctx, _cancel| {
        let s = args.to_string();
        Box::pin(async move { format!("echo:{s}") })
    });
    let entry = crate::ToolEntry {
        definition: fuyao_api::ToolDefinition::new("echo", "回显参数"),
        handler,
        child_invisible: false,
    };
    Arc::new(ToolRegistry::builder().register(entry).build())
}

/// 测试用 ModelConfig：携带 `test/...` 形式的 model_id
///
/// 必须带 model_id——否则 turn.rs::resolve_model 在 None + 无 [models.default] 时
/// 返回 Err，走配置错误分支结束 turn，测试 ReAct 行为就跑不起来。所有测试的
/// ProviderRegistry 都用 `with_instance("test", ...)` 构造，model_id 拆出的
/// provider_id = "test" 能匹配到。
fn test_params() -> fuyao_api::ModelConfig {
    fuyao_api::ModelConfig {
        model_id: Some("test/test-model".to_string()),
        thinking_type: None,
        reasoning_effort: None,
    }
}

/// 测试用 SessionParams（model_id 同 test_params，供 SessionCtx 构造用）
fn test_session_params() -> fuyao_api::SessionParams {
    fuyao_api::SessionParams {
        agent_config: fuyao_api::AgentConfig::default(),
        model_config: test_params(),
    }
}

/// 构造测试用入站消息（默认 Guide 模式 + User 来源）
///
/// 返回 output 侧 `OutputUserMessage`——内核统一处理输出侧消息，入站通道与队列
/// 载荷均为此类型。模型配置不再随消息携带，由 session 的 SessionParams 提供。
fn make_inbound(content: &str) -> OutputUserMessage {
    OutputUserMessage {
        base: EventBase::default(),
        payload: OutputUserPayload {
            content: content.to_string(),
            images: vec![],
            mode: UserMessageMode::Guide,
            source: UserMessageSource::User,
        },
    }
}

/// 构造测试用入站消息（指定 mode）
fn make_inbound_with_mode(content: &str, mode: UserMessageMode) -> OutputUserMessage {
    OutputUserMessage {
        base: EventBase::default(),
        payload: OutputUserPayload {
            content: content.to_string(),
            images: vec![],
            mode,
            source: UserMessageSource::User,
        },
    }
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
    rx_interrupt: Receiver<OutputInterruptMessage>,
    #[allow(dead_code)]
    tx_interrupt: mpsc::Sender<OutputInterruptMessage>,
    /// 控制通道接收端：run_turn 间隙检查点消费它
    rx_control: Receiver<ControlCommand>,
    /// 控制通道发送端：测试向 ReAct 间隙注入控制命令（回退 / 压缩）用
    tx_control: mpsc::Sender<ControlCommand>,
    rx_event: mpsc::UnboundedReceiver<OutputEvent>,
}

async fn make_harness(provider: Arc<dyn Provider>, tools: Arc<ToolRegistry>) -> TestHarness {
    make_harness_with_hooks(provider, tools, empty_hooks()).await
}

/// 同 make_harness，但允许传入自定义 hooks（用于拦截同步测试）
async fn make_harness_with_hooks(
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    hooks: fuyao_hooks::SharedHooks,
) -> TestHarness {
    make_harness_full(provider, tools, hooks, fuyao_api::AgentPaths::default()).await
}

/// 同 make_harness_with_hooks，但可指定 agent_paths
///
/// 全局模型缓存按 agent_paths 隔离——需要注册独立模型配置（如图片能力）的测试
/// 用独立的 agent_id 键，避免污染/被污染其他并行测试的缓存。
async fn make_harness_full(
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    hooks: fuyao_hooks::SharedHooks,
    agent_paths: fuyao_api::AgentPaths,
) -> TestHarness {
    let store = temp_store().await;
    let mut session = Session::new(None, None, Some("系统提示词".to_string()));
    // 强制 session.id 与 emitter 的 session_id 一致
    // （build_chat_request 用 emitter.session_id() 查 DB，必须匹配）
    session.id = "test_session".to_string();
    store.create(&session).await.unwrap();
    let (tx_event, rx_event) = mpsc::unbounded_channel();
    let (tx_interrupt, rx_interrupt) = mpsc::channel(8);
    // 控制通道：tx 保留供测试注入命令，rx 供 run_turn 间隙检查点 try_recv
    let (tx_control, rx_control) = mpsc::channel::<ControlCommand>(8);
    // 包成 ProviderRegistry：测试里所有 model_id 都用 "test/..."，统一走 test provider。
    // turn.rs 从 ctx.providers.get(provider_id) 取实例，必须找到才能继续。
    let providers = Arc::new(fuyao_provider::ProviderRegistry::with_instance(
        "test", provider,
    ));
    let ctx = SessionCtx {
        store,
        providers,
        tools,
        hooks,
        agent_paths,
        definition: fuyao_api::AgentDefinition::default(),
        session_params: Arc::new(tokio::sync::Mutex::new(test_session_params())),
        emitter: Emitter::new(tx_event, "test_session".to_string()),
        guide: empty_queue(),
        pending: empty_queue(),
        last_usage: Arc::new(tokio::sync::Mutex::new(None)),
        compression_config: fuyao_api::CompressionConfig::default(),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
        subagent_ops: None,
    };
    TestHarness {
        ctx,
        session,
        rx_interrupt,
        tx_interrupt,
        rx_control,
        tx_control,
        rx_event,
    }
}

/// 收集所有产出事件（直到通道暂时无数据）
async fn collect_events(rx: &mut mpsc::UnboundedReceiver<OutputEvent>) -> Vec<OutputEvent> {
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
        OutputEvent::Compression(m) => m.base.session_id.as_deref(),
        OutputEvent::Title(m) => m.base.session_id.as_deref(),
        OutputEvent::Retry(m) => m.base.session_id.as_deref(),
        OutputEvent::ChildSession(m) => m.base.session_id.as_deref(),
        OutputEvent::Rollback(m) => m.base.session_id.as_deref(),
    }
}

/// 预置一条 user 消息进 DB（模拟主循环 inject 后的状态）
///
/// 消息已不在内存（事件级落库），直接调 store.insert_message 落 DB。
/// 同步维护 session.message_count（emit_to_history 正常路径也会 +1，
/// preload 走捷径直接 insert，需手动同步计数器避免后续 persist/update 时计数丢失）。
async fn preload_user(h: &mut TestHarness, content: &str) {
    let mut msg = fuyao_api::Message::user(content.to_string());
    h.ctx
        .store
        .insert_message(&h.session.id, &mut msg)
        .await
        .expect("preload 落库失败");
    h.session.message_count += 1;
}

/// 从 DB 加载可见消息（事件级落库模式下消息不在内存）
async fn visible_messages(h: &TestHarness) -> Vec<fuyao_api::Message> {
    h.ctx
        .store
        .load_visible_messages(&h.session.id, usize::MAX)
        .await
        .expect("加载可见消息失败")
}

/// 无工具单轮：user → 流式回复 → AssistantMessage
#[tokio::test]
async fn single_turn_no_tools() {
    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response("你好")]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&mut h, "用户问题").await;

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
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
    // user + assistant（事件级落库，从 DB 查询验证）
    let msgs = visible_messages(&h).await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, MessageRole::User);
    assert_eq!(msgs[1].role, MessageRole::Assistant);
}

/// 有工具循环：先工具调用 → 执行 echo → 再最终回复
#[tokio::test]
async fn react_loop_with_tool() {
    let provider = Arc::new(MockProvider::new(vec![
        MockProvider::tool_call_response("call_1", "echo", r#"{"msg":"hi"}"#),
        MockProvider::text_response("工具执行完毕"),
    ]));
    let mut h = make_harness(provider, echo_registry()).await;
    preload_user(&mut h, "调工具").await;

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;

    let events = collect_events(&mut h.rx_event).await;
    let has_tool_result = events
        .iter()
        .any(|e| matches!(e, OutputEvent::ToolResult(m) if m.payload.tool_name == "echo"));
    assert!(has_tool_result, "应有 ToolResult 事件");
    let msgs = visible_messages(&h).await;
    assert!(msgs.len() >= 3, "应含 user/assistant/tool 至少 3 条");
    let last = msgs.last().unwrap();
    assert_eq!(last.role, MessageRole::Assistant);
}

/// 工具结果进 messages（tool_call_id 回填）
#[tokio::test]
async fn tool_result_in_messages() {
    let provider = Arc::new(MockProvider::new(vec![
        MockProvider::tool_call_response("tc_42", "echo", r#"{"x":1}"#),
        MockProvider::text_response("完成"),
    ]));
    let mut h = make_harness(provider, echo_registry()).await;
    preload_user(&mut h, "test").await;

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;

    let msgs = visible_messages(&h).await;
    let tool_msg = msgs
        .iter()
        .find(|m| matches!(m.role, MessageRole::Tool))
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
    preload_user(&mut h, "test").await;

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
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
    preload_user(&mut h, "原始问题").await;
    // 工具执行期间用户补充 2 条 guide 消息（模拟入队）
    h.ctx.guide.lock().unwrap().push_back(make_inbound("补充1"));
    h.ctx.guide.lock().unwrap().push_back(make_inbound("补充2"));

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;

    // 可见消息应含：原始user + assistant(tool_calls) + tool + 补充1 + 补充2 + assistant(最终)
    let user_msgs: Vec<_> = visible_messages(&h)
        .await
        .iter()
        .filter(|m| matches!(m.role, MessageRole::User))
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
    preload_user(&mut h, "第一条").await;
    // 预置 pending 1 条 + guide 1 条（最终回复后应都被消费）
    h.ctx
        .pending
        .lock()
        .unwrap()
        .push_back(make_inbound_with_mode("排队消息", UserMessageMode::Pending));
    h.ctx
        .guide
        .lock()
        .unwrap()
        .push_back(make_inbound_with_mode("引导消息", UserMessageMode::Guide));

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;

    // 两条都应进 DB
    let user_msgs: Vec<_> = visible_messages(&h)
        .await
        .iter()
        .filter(|m| matches!(m.role, MessageRole::User))
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
    preload_user(&mut h, "问题").await;

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;

    // user + assistant，没有多余消息
    let msgs = visible_messages(&h).await;
    assert_eq!(msgs.len(), 2);
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
    let mut session = Session::new(None, None, Some("系统提示词".to_string()));
    session.id = "test_session".to_string();
    store.create(&session).await.unwrap();

    let guide = empty_queue();
    let pending = empty_queue();
    // 入站通道（User 消息经此送进 session task 过管道入队）
    let (tx_inbound, rx_inbound) = mpsc::channel::<OutputUserMessage>(16);
    // tx 必须随测试存活以保持中断通道打开（rx_interrupt.recv() 不提前返回 None）
    let _tx_interrupt = mpsc::channel::<OutputInterruptMessage>(8).0;
    let (rx_interrupt_tx, rx_interrupt) = mpsc::channel::<OutputInterruptMessage>(8);
    std::mem::forget(rx_interrupt_tx);
    // Plugin 通道（保持打开，避免 rx_plugin.recv() 提前返回 None）
    let (_tx_plugin, rx_plugin) = mpsc::channel::<OutputPluginMessage>(16);
    // 控制通道（保持打开，避免 rx_control.recv() 提前返回 None）
    let (_tx_control, rx_control) = mpsc::channel::<ControlCommand>(8);
    let (tx_event, mut rx_event) = mpsc::unbounded_channel();

    // 启动 session 执行流（两队列都空，task 进入 select! 等待）
    let providers = Arc::new(fuyao_provider::ProviderRegistry::with_instance(
        "test", provider,
    ));
    let task = tokio::spawn(run_session(
        "test_session".to_string(),
        Arc::clone(&guide),
        Arc::clone(&pending),
        rx_inbound,
        rx_interrupt,
        rx_plugin,
        rx_control,
        tokio_util::sync::CancellationToken::new(),
        session,
        Arc::clone(&store),
        providers,
        Arc::new(ToolRegistry::builder().build()),
        empty_hooks(),
        fuyao_api::AgentPaths::default(),
        fuyao_api::AgentDefinition::default(),
        Arc::new(tokio::sync::Mutex::new(test_session_params())),
        tx_event,
        None,
    ));

    // 模拟 send：经入站通道发一条 Pending 消息（过管道入 pending 队列）
    // task 空闲（无活跃 ReAct 链），pending 的解禁条件已满足
    tx_inbound
        .send(make_inbound_with_mode(
            "排队消息",
            fuyao_api::UserMessageMode::Pending,
        ))
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

/// turn 结束（队列跑空）后停止消费，新用户消息（inbound）重新启动 turn。
///
/// 验证停止消费 + 启动分离的核心机制：
/// 1. 第一条 inbound → turn 跑完 → run_turn return → 进 select! 等待（停止消费）
/// 2. 第二条 inbound → 重新启动 → 第二个 turn 跑完
///
/// 修复前 run_turn return 后无条件回顶部 consume，但因队列已空本就进 select!，
/// 此测试主要守护「停止后 inbound 能重新启动」的链路不被破坏。
#[tokio::test]
async fn turn_restart_on_new_inbound_after_drained() {
    let provider = Arc::new(MockProvider::new(vec![
        MockProvider::text_response("回复1"),
        MockProvider::text_response("回复2"),
    ]));

    let store = temp_store().await;
    let mut session = Session::new(None, None, Some("系统提示词".to_string()));
    session.id = "test_session".to_string();
    store.create(&session).await.unwrap();

    let guide = empty_queue();
    let pending = empty_queue();
    let (tx_inbound, rx_inbound) = mpsc::channel::<OutputUserMessage>(16);
    let _tx_interrupt = mpsc::channel::<OutputInterruptMessage>(8).0;
    let (rx_interrupt_tx, rx_interrupt) = mpsc::channel::<OutputInterruptMessage>(8);
    std::mem::forget(rx_interrupt_tx);
    let (_tx_plugin, rx_plugin) = mpsc::channel::<OutputPluginMessage>(16);
    let (_tx_control, rx_control) = mpsc::channel::<ControlCommand>(8);
    let (tx_event, mut rx_event) = mpsc::unbounded_channel();

    let providers = Arc::new(fuyao_provider::ProviderRegistry::with_instance(
        "test", provider,
    ));
    let task = tokio::spawn(run_session(
        "test_session".to_string(),
        Arc::clone(&guide),
        Arc::clone(&pending),
        rx_inbound,
        rx_interrupt,
        rx_plugin,
        rx_control,
        tokio_util::sync::CancellationToken::new(),
        session,
        Arc::clone(&store),
        providers,
        Arc::new(ToolRegistry::builder().build()),
        empty_hooks(),
        fuyao_api::AgentPaths::default(),
        fuyao_api::AgentDefinition::default(),
        Arc::new(tokio::sync::Mutex::new(test_session_params())),
        tx_event,
        None,
    ));

    // 第一条消息 → 第一个 turn → 收到「回复1」
    tx_inbound.send(make_inbound("问题1")).await.unwrap();
    let got_first = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(ev) = rx_event.recv().await {
            if matches!(ev, OutputEvent::Assistant(m) if m.payload.content.as_deref() == Some("回复1"))
            {
                return true;
            }
        }
        false
    })
    .await;
    assert!(
        got_first.unwrap_or(false),
        "应收到第一条消息的回复「回复1」"
    );

    // 第二条消息 → 重新启动 → 收到「回复2」（证明停止后 inbound 能重新启动）
    tx_inbound.send(make_inbound("问题2")).await.unwrap();
    let got_second = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(ev) = rx_event.recv().await {
            if matches!(ev, OutputEvent::Assistant(m) if m.payload.content.as_deref() == Some("回复2"))
            {
                return true;
            }
        }
        false
    })
    .await;

    task.abort();

    assert!(
        got_second.unwrap_or(false),
        "停止后发新消息应重新启动 turn 并收到「回复2」"
    );
}

/// Plugin 消息路由：经 tx_plugin 通道发 output 侧 PluginMessage（Engine::send
/// 已在入口转化） → 从 rx_event 流出 OutputEvent::Plugin（session_id 标签正确）。
///
/// 验证链路：
/// - tx_plugin 通道承载 output 侧 PluginMessage（入口转化后内核只认 output 侧）
/// - session task 过 dispatch 管道，经 Emitter 自动盖 session_id 标签
#[tokio::test]
async fn plugin_message_routes_through_dispatch() {
    use fuyao_api::PluginEventSource;
    use fuyao_api::message::output::{PluginMessage, PluginPayload};

    // 不会被调用（Plugin 消息不触发 ReAct），随便给个空响应占位
    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response("ok")]));

    let store = temp_store().await;
    let mut session = Session::new(None, None, Some("系统提示词".to_string()));
    session.id = "plugin_session".to_string();
    store.create(&session).await.unwrap();

    let guide = empty_queue();
    let pending = empty_queue();
    let (_tx_inbound, rx_inbound) = mpsc::channel::<OutputUserMessage>(16);
    let _tx_interrupt = mpsc::channel::<OutputInterruptMessage>(8).0;
    let (rx_interrupt_tx, rx_interrupt) = mpsc::channel::<OutputInterruptMessage>(8);
    std::mem::forget(rx_interrupt_tx);
    // tx_plugin 需要保留以发送消息
    let (tx_plugin, rx_plugin) = mpsc::channel::<OutputPluginMessage>(16);
    // 控制通道（保持打开，避免 rx_control.recv() 提前返回 None）
    let (_tx_control, rx_control) = mpsc::channel::<ControlCommand>(8);
    let (tx_event, mut rx_event) = mpsc::unbounded_channel();

    let providers = Arc::new(fuyao_provider::ProviderRegistry::with_instance(
        "test", provider,
    ));
    let task = tokio::spawn(run_session(
        "plugin_session".to_string(),
        Arc::clone(&guide),
        Arc::clone(&pending),
        rx_inbound,
        rx_interrupt,
        rx_plugin,
        rx_control,
        tokio_util::sync::CancellationToken::new(),
        session,
        Arc::clone(&store),
        providers,
        Arc::new(ToolRegistry::builder().build()),
        empty_hooks(),
        fuyao_api::AgentPaths::default(),
        fuyao_api::AgentDefinition::default(),
        Arc::new(tokio::sync::Mutex::new(test_session_params())),
        tx_event,
        None,
    ));

    // 模拟 Engine::send 入口转化后送入 tx_plugin 通道的 output 侧 PluginMessage
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

/// Plugin 通知在**活跃 turn 期间**也能立即转发（不到等回 idle）。
///
/// 这是本次修复的核心回归保护：修复前 rx_plugin 仅在主循环 idle select! 消费，
/// turn 运行（LLM 流式 + 工具执行）期间通知堆在通道里，延迟整轮。
/// 修复后由独立 forwarder task 并发消费，turn 挂起在流上时通知仍即时透传。
///
/// 时序：ControllableProvider 吐一个 TextDelta 后流挂起（turn 仍活跃）→
/// 发 Plugin 消息 → 断言 500ms 内收到 OutputEvent::Plugin。
/// 修复前此断言会超时（流不结束 = 主循环不回 idle = 永不消费 plugin）。
#[tokio::test]
async fn plugin_forwards_during_active_turn() {
    use fuyao_api::PluginEventSource;
    use fuyao_api::message::output::{PluginMessage, PluginPayload};

    // 1 轮事件流：吐一个 TextDelta 后挂起（第二个事件不发 → turn 停在流式 select!）
    let (provider, txs) = ControllableProvider::with_batches(1);
    let provider: Arc<dyn Provider> = Arc::new(provider);

    let store = temp_store().await;
    let mut session = Session::new(None, None, Some("系统提示词".to_string()));
    session.id = "plugin_active".to_string();
    store.create(&session).await.unwrap();

    let guide = empty_queue();
    let pending = empty_queue();
    // 预置一条 user 进 guide，让主循环进 turn（不需经 inbound 通道）
    guide.lock().unwrap().push_back(make_inbound("问题"));
    let (_tx_inbound, rx_inbound) = mpsc::channel::<OutputUserMessage>(16);
    let _tx_interrupt = mpsc::channel::<OutputInterruptMessage>(8).0;
    let (rx_interrupt_tx, rx_interrupt) = mpsc::channel::<OutputInterruptMessage>(8);
    std::mem::forget(rx_interrupt_tx);
    let (tx_plugin, rx_plugin) = mpsc::channel::<OutputPluginMessage>(16);
    // 控制通道（保持打开，避免 rx_control.recv() 提前返回 None）
    let (_tx_control, rx_control) = mpsc::channel::<ControlCommand>(8);
    let (tx_event, mut rx_event) = mpsc::unbounded_channel();

    let providers = Arc::new(fuyao_provider::ProviderRegistry::with_instance(
        "test", provider,
    ));
    let shutdown_token = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(run_session(
        "plugin_active".to_string(),
        Arc::clone(&guide),
        Arc::clone(&pending),
        rx_inbound,
        rx_interrupt,
        rx_plugin,
        rx_control,
        tokio_util::sync::CancellationToken::new(),
        session,
        Arc::clone(&store),
        providers,
        Arc::new(ToolRegistry::builder().build()),
        empty_hooks(),
        fuyao_api::AgentPaths::default(),
        fuyao_api::AgentDefinition::default(),
        Arc::new(tokio::sync::Mutex::new(test_session_params())),
        tx_event,
        None,
    ));

    // 喂一个 TextDelta：run_session 进 turn，流式消费后挂起在第二个事件上（turn 活跃）
    txs[0]
        .send(Ok(StreamEvent::TextDelta {
            content: "部分".to_string(),
        }))
        .unwrap();
    // 等 turn 跑过 inject + build_chat_request(DB 查询) + 进入流式挂起
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    // 活跃 turn 期间发 Plugin 消息
    tx_plugin
        .send(PluginMessage {
            base: EventBase::default(),
            payload: PluginPayload {
                source: PluginEventSource {
                    name: "test_plugin".into(),
                },
                event_type: "notify".into(),
                data: None,
                error: None,
                message: Some("turn 中转发".into()),
            },
        })
        .await
        .unwrap();

    // 断言：500ms 内收到 Plugin 事件（修复前会延迟整轮，流挂起=主循环永不回 idle=超时）
    let got = tokio::time::timeout(std::time::Duration::from_millis(500), async {
        loop {
            if let Some(ev) = rx_event.recv().await
                && let OutputEvent::Plugin(m) = ev
            {
                assert_eq!(m.payload.source.name, "test_plugin");
                assert_eq!(
                    m.base.session_id.as_deref(),
                    Some("plugin_active"),
                    "Plugin 事件应盖 session_id 标签"
                );
                return;
            }
        }
    })
    .await;

    // 收尾：cancel shutdown 让挂起的 turn 经 shutdown 分支退出，再等 task 结束
    shutdown_token.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), task).await;

    assert!(
        got.is_ok(),
        "活跃 turn 期间 Plugin 事件应在 500ms 内转发；修复前会延迟整轮"
    );
}

/// 中断-流式期间：流式中途发 Interrupt → 产出 Interrupt 事件 + 部分 AssistantMessage（finish_reason=interrupted）
///
/// 时序：ControllableProvider 吐一个 TextDelta 后故意不发下一个（流挂起），
/// 此时发中断信号，run_turn 的 select!（turn.rs:64）中断分支胜出。
/// handle_interrupt 读 TurnState 已累积的"你好"发部分 AssistantMessage。
#[tokio::test]
async fn interrupt_during_streaming() {
    use fuyao_api::InterruptSource;
    use fuyao_api::message::output::InterruptMessage;

    // 准备 1 轮事件流（中断发生在首轮流式期间）
    let (provider, txs) = ControllableProvider::with_batches(1);
    let provider: Arc<dyn Provider> = Arc::new(provider);
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&mut h, "test").await;

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
        &mut h.rx_control,
        test_params(),
    );
    tokio::pin!(turn_fut);
    let interrupter = async {
        // 等 run_turn 跑起来并挂起在流的第二个事件上
        // 事件级落库模式下 run_turn 启动路径多了 DB 查询（build_chat_request），
        // yield_now 次数比内存模式多给一些，确保 TextDelta 已消费到 TurnState
        for _ in 0..6 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        tx_interrupt
            .send(InterruptMessage::new("用户取消", InterruptSource::User))
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
    use fuyao_api::InterruptSource;
    use fuyao_api::message::output::InterruptMessage;

    let provider = Arc::new(MockProvider::new(vec![MockProvider::tool_call_response(
        "tc_block",
        "blocking_tool",
        r#"{}"#,
    )]));

    // 注册阻塞工具：handler 等一个永不到来的信号，确保中断前不会完成
    let blocking_handler: fuyao_api::ToolFn = Arc::new(|_args, _ctx, _cancel| {
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
            child_invisible: false,
        })
        .build();

    let mut h = make_harness(provider, Arc::new(tools)).await;
    preload_user(&mut h, "调工具").await;

    let tx_interrupt = h.tx_interrupt.clone();

    let turn_fut = turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    );
    tokio::pin!(turn_fut);
    let interrupter = async {
        // 等 run_turn 跑完流式（工具调用）并进入工具执行阻塞
        for _ in 0..6 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        tx_interrupt
            .send(InterruptMessage::new("用户取消", InterruptSource::User))
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

/// shutdown-流式期间：ControllableProvider 吐 TextDelta 后挂起 → cancel shutdown_token
///
/// 验证 turn.rs 流式 select! 的 shutdown 分支（biased 优先）：
/// - 收到 Interrupt 事件（source=Shutdown，reason=引擎关闭）
/// - 补发部分 AssistantMessage（finish_reason=interrupted，含已累积文本）
/// - turn 在 2s 内结束（不依赖 10s abort 兜底）
#[tokio::test]
async fn shutdown_during_streaming() {
    // 准备 1 轮事件流（shutdown 发生在首轮流式期间）
    let (provider, txs) = ControllableProvider::with_batches(1);
    let provider: Arc<dyn Provider> = Arc::new(provider);
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&mut h, "test").await;

    // 先 clone shutdown_token，turn_fut 借用 &h.ctx 后仍能在 interrupter 里 cancel
    let shutdown_token = h.ctx.shutdown_token.clone();

    // 喂一个 TextDelta（run_turn 启动后消费，进入流式 select! 挂起在第二个事件上）
    txs[0]
        .send(Ok(StreamEvent::TextDelta {
            content: "你好".to_string(),
        }))
        .unwrap();

    let turn_fut = turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    );
    tokio::pin!(turn_fut);
    let canceller = async {
        // 等 run_turn 跑起来并挂起在流的第二个事件上
        // 事件级落库模式下启动路径含 DB 查询，yield_now 多给一些
        for _ in 0..6 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        shutdown_token.cancel();
    };
    tokio::select! {
        _ = &mut turn_fut => {}
        _ = canceller => {
            // shutdown 信号已发，等 turn_fut 自己因 select! shutdown 分支结束
            tokio::time::timeout(std::time::Duration::from_secs(2), turn_fut)
                .await
                .expect("run_turn 应在 shutdown 后立即结束（不依赖 10s abort）");
        }
    }

    let events = collect_events(&mut h.rx_event).await;
    // 断言 Interrupt 通知事件（source=Shutdown）
    let interrupt = events.iter().find_map(|e| match e {
        OutputEvent::Interrupt(m) => Some(m),
        _ => None,
    });
    let interrupt = interrupt.expect("应有 Interrupt 事件");
    assert_eq!(
        interrupt.payload.source,
        fuyao_api::message::input::InterruptSource::Shutdown,
        "shutdown 触发的中断 source 应为 Shutdown"
    );
    assert_eq!(interrupt.payload.reason, "引擎关闭");
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

/// shutdown-工具执行期间：阻塞式工具 handler 卡住 → cancel shutdown_token
///
/// 验证 turn.rs 工具执行 select! 的 shutdown 分支（biased 优先）：
/// - 收到 Interrupt 事件（source=Shutdown）
/// - 已完成的工具结果不丢（push 进 history）
/// - 未完成的 tool_call 补发中断式 ToolResult（content 含 [Shutdown][引擎关闭]）
/// - turn 在 2s 内结束（不依赖 10s abort 兜底，工具 handler sleep 30s 也不会卡住）
#[tokio::test]
async fn shutdown_during_tool_execution() {
    let provider = Arc::new(MockProvider::new(vec![MockProvider::tool_call_response(
        "tc_shutdown",
        "blocking_tool",
        r#"{}"#,
    )]));

    // 注册阻塞工具：handler 等一个永不到来的信号，确保 shutdown 前不会完成
    let blocking_handler: fuyao_api::ToolFn = Arc::new(|_args, _ctx, _cancel| {
        Box::pin(async {
            // 永不完成：sleep 30 秒，足够测试发 shutdown 信号
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            "unreachable".to_string()
        })
    });
    let tools = ToolRegistry::builder()
        .register(crate::ToolEntry {
            definition: fuyao_api::ToolDefinition::new("blocking_tool", "阻塞测试工具"),
            handler: blocking_handler,
            child_invisible: false,
        })
        .build();

    let mut h = make_harness(provider, Arc::new(tools)).await;
    preload_user(&mut h, "调工具").await;

    let shutdown_token = h.ctx.shutdown_token.clone();

    let turn_fut = turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    );
    tokio::pin!(turn_fut);
    let canceller = async {
        // 等 run_turn 跑完流式（工具调用）并进入工具执行阻塞
        for _ in 0..6 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        shutdown_token.cancel();
    };
    tokio::select! {
        _ = &mut turn_fut => {}
        _ = canceller => {
            tokio::time::timeout(std::time::Duration::from_secs(2), turn_fut)
                .await
                .expect("run_turn 应在 shutdown 后立即结束（工具 handler sleep 30s 也不应卡住）");
        }
    }

    let events = collect_events(&mut h.rx_event).await;
    // 断言 Interrupt 事件 source=Shutdown
    let interrupt = events.iter().find_map(|e| match e {
        OutputEvent::Interrupt(m) => Some(m),
        _ => None,
    });
    let interrupt = interrupt.expect("应有 Interrupt 事件");
    assert_eq!(
        interrupt.payload.source,
        fuyao_api::message::input::InterruptSource::Shutdown
    );
    // 中断式 ToolResult：content 格式 [{source:?}][{reason}]
    let tool_result = events.iter().find_map(|e| match e {
        OutputEvent::ToolResult(m) if m.payload.tool_name == "blocking_tool" => Some(m),
        _ => None,
    });
    let tool_result = tool_result.expect("应有 blocking_tool 的中断式 ToolResult");
    assert_eq!(tool_result.payload.tool_call_id, "tc_shutdown");
    assert!(
        tool_result.payload.content.contains("Shutdown"),
        "中断式 ToolResult content 应含 Shutdown 来源: {}",
        tool_result.payload.content
    );
    assert!(
        tool_result.payload.content.contains("引擎关闭"),
        "中断式 ToolResult content 应含「引擎关闭」原因: {}",
        tool_result.payload.content
    );
}

/// 落库验证：跑完一轮含工具调用的 ReAct，从 DB 重新加载 messages 完整持久化
///
/// 验证事件级落库（emit_to_history → store.insert_message）确实把完整消息写进 DB。
/// 事件级落库模式下消息产生即落库，从 store.load_visible_messages 重新查验证完整性。
#[tokio::test]
async fn messages_persisted_to_db() {
    let provider = Arc::new(MockProvider::new(vec![
        MockProvider::tool_call_response("tc_99", "echo", r#"{"msg":"hi"}"#),
        MockProvider::text_response("完成"),
    ]));
    let mut h = make_harness(provider, echo_registry()).await;
    preload_user(&mut h, "调工具").await;
    let session_id = h.session.id.clone();

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;

    // 从 DB 重新加载可见消息，验证持久化的完整性
    let reloaded = h
        .ctx
        .store
        .load_visible_messages(&session_id, usize::MAX)
        .await
        .expect("load_visible_messages 不应失败");
    assert!(
        reloaded.len() >= 4,
        "应含 user/assistant(tool_calls)/tool/assistant(最终) 至少 4 条，实际 {}",
        reloaded.len()
    );

    // 验证关键消息类型完整保留
    let roles: Vec<_> = reloaded.iter().map(|m| m.role.as_str()).collect();
    assert!(roles.contains(&"user"), "应含 user 消息");
    assert!(roles.contains(&"tool"), "应含 tool 消息");
    assert!(
        roles.iter().filter(|r| **r == "assistant").count() >= 2,
        "应含至少 2 条 assistant"
    );

    // 验证 tool 消息的 tool_call_id 回填正确
    let tool_msg = reloaded
        .iter()
        .find(|m| matches!(m.role, MessageRole::Tool))
        .expect("应有 tool 消息");
    assert_eq!(
        tool_msg.tool_call_id.as_deref(),
        Some("tc_99"),
        "tool 消息应回填正确的 tool_call_id"
    );

    // 验证 session.message_count 计数器与 DB 行数一致（事件级落库计数器维护）
    let reloaded_session = h
        .ctx
        .store
        .get(&session_id)
        .await
        .expect("get 不应失败")
        .expect("session 应已落库");
    assert_eq!(
        reloaded_session.message_count as usize,
        reloaded.len(),
        "message_count 应与可见消息数一致"
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
    preload_user(&mut h, "提问").await;

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
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

/// 费用统计：每条 assistant 消息（无论是否含工具调用）都应累积进 session 总计，
/// 持久化的 Message 字段也应携带 token（修复重构漏搬：之前落库 token 全为 0）。
///
/// 两轮 ReAct（工具调用轮 + 最终回复轮），每轮 mock 不同 usage：
/// - 工具调用轮：prompt=100, completion=50, reasoning=10, cached=20
/// - 最终回复轮：prompt=200, completion=80, reasoning=20, cached=40
/// 预期累积：prompt=300, completion=130, reasoning=30, cached=60
///
/// 同时验证 cost 字段被赋值——测试前向全局模型注册表注入带价格的测试模型，
/// cost 应为非零（按价格表算），结束后清理缓存避免污染其他测试。
#[tokio::test]
async fn cost_accumulated_per_assistant_message() {
    use fuyao_api::{Model, ModelCost, ModelLimit, ModelModalities};

    // 构造带价格的测试模型并注册到全局缓存
    // 输入 2/M、输出 12/M、推理 6/M、缓存 0.4/M（与 cost.rs 单测一致）
    let test_model = Model {
        name: "cost-test".to_string(),
        cost: ModelCost {
            input: Some(2.0),
            output: Some(12.0),
            reasoning: Some(6.0),
            cache: Some(0.4),
            tiers: vec![],
        },
        limit: ModelLimit::default(),
        reasoning_efforts: vec![],
        modalities: ModelModalities::default(),
    };
    let agent_paths = fuyao_api::AgentPaths::default();
    let cache_key = fuyao_provider::agent_paths_cache_key(&agent_paths);
    fuyao_provider::register_model("test/cost-model", test_model, &cache_key);

    // 用带 usage 的两轮响应（工具调用 + 最终回复）
    let tool_usage = StreamUsage {
        prompt_tokens: 100,
        completion_tokens: 50,
        total_tokens: 150,
        completion_reasoning_tokens: Some(10),
        prompt_cached_tokens: Some(20),
    };
    let final_usage = StreamUsage {
        prompt_tokens: 200,
        completion_tokens: 80,
        total_tokens: 280,
        completion_reasoning_tokens: Some(20),
        prompt_cached_tokens: Some(40),
    };
    let provider = Arc::new(MockProvider::new(vec![
        MockProvider::tool_call_response_with_usage("c_cost", "echo", r#"{}"#, tool_usage),
        MockProvider::text_response_with_usage("done", final_usage),
    ]));

    let mut h = make_harness(provider, echo_registry()).await;
    preload_user(&mut h, "测费用累积").await;

    // 用带 model_id 的 model_config，让累积逻辑能查到价格表
    let mut params = test_params();
    params.model_id = Some("test/cost-model".to_string());

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        params,
    )
    .await;

    // 清理全局缓存（避免污染后续测试）
    fuyao_provider::clear_cache(&agent_paths);

    // === 验证 1：session 总计正确累积（两轮相加） ===
    assert_eq!(
        h.session.total_prompt_tokens, 300,
        "工具调用轮(100) + 最终回复轮(200) = 300"
    );
    assert_eq!(
        h.session.total_completion_tokens, 130,
        "工具调用轮(50) + 最终回复轮(80) = 130"
    );
    assert_eq!(
        h.session.total_reasoning_tokens, 30,
        "工具调用轮(10) + 最终回复轮(20) = 30"
    );
    assert_eq!(
        h.session.total_cached_tokens, 60,
        "工具调用轮(20) + 最终回复轮(40) = 60"
    );

    // === 验证 2：cost 为非零（价格表已注入，按 /M 算） ===
    assert!(
        h.session.total_cost > 0.0,
        "注入价格表后 session.total_cost 应非零，实际 = {}",
        h.session.total_cost
    );

    // === 验证 3：持久化的 Message 字段携带 token（usage 持久化洞修复） ===
    let assistant_msgs: Vec<_> = visible_messages(&h)
        .await
        .iter()
        .filter(|m| matches!(m.role, MessageRole::Assistant))
        .cloned()
        .collect();
    assert_eq!(
        assistant_msgs.len(),
        2,
        "应有两条 assistant 消息（工具调用轮 + 最终回复轮）"
    );

    // 第一条：工具调用轮
    let tool_turn = assistant_msgs
        .iter()
        .find(|m| m.finish_reason.as_deref() == Some("tool_calls"))
        .expect("应有 finish_reason=tool_calls 的消息");
    assert_eq!(tool_turn.prompt_tokens, 100);
    assert_eq!(tool_turn.completion_tokens, 50);
    assert_eq!(tool_turn.reasoning_tokens, 10);
    assert_eq!(tool_turn.cached_tokens, 20);
    assert!(
        tool_turn.cost > 0.0,
        "工具调用轮 Message.cost 应非零，实际 = {}",
        tool_turn.cost
    );

    // 第二条：最终回复轮
    let final_turn = assistant_msgs
        .iter()
        .find(|m| m.finish_reason.as_deref() == Some("stop"))
        .expect("应有 finish_reason=stop 的消息");
    assert_eq!(final_turn.prompt_tokens, 200);
    assert_eq!(final_turn.completion_tokens, 80);
    assert_eq!(final_turn.reasoning_tokens, 20);
    assert_eq!(final_turn.cached_tokens, 40);
    assert!(
        final_turn.cost > 0.0,
        "最终回复轮 Message.cost 应非零，实际 = {}",
        final_turn.cost
    );

    // === 验证 4：两条 Message.cost 之和 = session.total_cost（无丢失） ===
    let sum_costs = tool_turn.cost + final_turn.cost;
    let diff = (sum_costs - h.session.total_cost).abs();
    assert!(
        diff < 1e-9,
        "两条 Message.cost({sum_costs}) 之和应等于 session.total_cost({})",
        h.session.total_cost
    );
}

// ===== emit_to_history 端到端拦截同步测试 =====

use fuyao_hooks::{HooksRegistry, InterceptResult};

/// 拦截修改最终 Assistant content 后：
/// - DB 里的 Message 携带修改后内容
/// - 下轮 build_chat_request 用的是修改后内容（拦截→存储→消费三者一致）
#[tokio::test]
async fn intercept_modifies_final_assistant_in_history_and_next_request() {
    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response(
        "原始内容",
    )]));
    let tools = Arc::new(ToolRegistry::builder().build());

    // 注册拦截器：给 Assistant content 加前缀 "[脱敏]"
    let hooks: fuyao_hooks::SharedHooks =
        Arc::new(tokio::sync::Mutex::new(HooksRegistry::default()));
    {
        let mut reg = hooks.lock().await;
        reg.register_output_intercept(
            0,
            Arc::new(|ev| {
                if let OutputEvent::Assistant(m) = ev {
                    let mut modified = m.clone();
                    if let Some(c) = &mut modified.payload.content {
                        *c = format!("[脱敏]{c}");
                    }
                    InterceptResult::Pass(OutputEvent::Assistant(modified))
                } else {
                    InterceptResult::Pass(ev.clone())
                }
            }),
        );
    }

    let mut h = make_harness_with_hooks(provider, tools, hooks).await;
    preload_user(&mut h, "用户问题").await;

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;

    // 1. DB 最后一条是修改后的内容
    let msgs = visible_messages(&h).await;
    let last_msg = msgs.last().expect("应有 assistant 消息进 DB");
    assert_eq!(last_msg.role, MessageRole::Assistant);
    assert_eq!(
        last_msg.content.as_deref(),
        Some("[脱敏]原始内容"),
        "DB 应携带拦截后的内容"
    );

    // 2. 下轮 build_chat_request 用的是修改后内容（端到端一致性）
    let request = super::builders::build_chat_request(
        h.ctx.store.as_ref(),
        &h.session.id,
        h.session.system_prompt.as_deref(),
        usize::MAX,
    )
    .await;
    let assistant_in_request = request
        .messages
        .iter()
        .rfind(|m| matches!(m.role, MessageRole::Assistant))
        .expect("ChatRequest 应包含 assistant 消息");
    assert_eq!(
        assistant_in_request.content.as_deref(),
        Some("[脱敏]原始内容"),
        "下轮 LLM 请求应使用拦截后的内容（拦截→存储→消费一致）"
    );
}

/// 拦截 Block 最终 Assistant 后：DB 不含 assistant 消息（不计费、不进历史）
#[tokio::test]
async fn intercept_block_skips_final_assistant_in_history() {
    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response(
        "被拦截",
    )]));
    let tools = Arc::new(ToolRegistry::builder().build());

    let hooks: fuyao_hooks::SharedHooks =
        Arc::new(tokio::sync::Mutex::new(HooksRegistry::default()));
    {
        let mut reg = hooks.lock().await;
        reg.register_output_intercept(
            0,
            Arc::new(|ev| {
                if matches!(ev, OutputEvent::Assistant(_)) {
                    InterceptResult::Block("拦截 assistant".to_string())
                } else {
                    InterceptResult::Pass(ev.clone())
                }
            }),
        );
    }

    let mut h = make_harness_with_hooks(provider, tools, hooks).await;
    preload_user(&mut h, "用户问题").await;

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;

    // Block：不应有任何 assistant 消息进 DB（只有 preload 的 user）
    let msgs = visible_messages(&h).await;
    let has_assistant = msgs
        .iter()
        .any(|m| matches!(m.role, MessageRole::Assistant));
    assert!(!has_assistant, "Block 时 assistant 消息不应进 DB");
    // total_cost 也应为 0（拦截 Block 的消息不计费）
    assert_eq!(h.session.total_cost, 0.0, "Block 时不应累积任何费用");
}

/// 用户消息经 inject_messages 时走 emit_to_history：插件可在**消费时刻**拦截改写。
///
/// 这是任务 4（01.3 文档）的核心修复验证：拦截/push/发送三时机对齐在消费时刻，
/// 与 assistant / tool_result 完全对称。修复前 inject_messages 是裸 push，插件
/// 无法在 user 消息进历史时介入（拦截裂缝）。
#[tokio::test]
async fn inject_messages_intercepts_user_at_consume_time() {
    let (tx_event, _rx_event) = mpsc::unbounded_channel::<OutputEvent>();
    let emitter = Emitter::new(tx_event, "test_session".to_string());
    let hooks: fuyao_hooks::SharedHooks =
        Arc::new(tokio::sync::Mutex::new(HooksRegistry::default()));
    {
        let mut reg = hooks.lock().await;
        reg.register_output_intercept(
            0,
            Arc::new(|ev| {
                if let OutputEvent::User(m) = ev {
                    let mut modified = m.clone();
                    modified.payload.content = format!("[脱敏]{}", modified.payload.content);
                    InterceptResult::Pass(OutputEvent::User(modified))
                } else {
                    InterceptResult::Pass(ev.clone())
                }
            }),
        );
    }
    let store = temp_store().await;
    let ctx = SessionCtx {
        store,
        providers: Arc::new(fuyao_provider::ProviderRegistry::with_instance(
            "test",
            Arc::new(MockProvider::new(vec![])),
        )),
        tools: Arc::new(ToolRegistry::builder().build()),
        hooks,
        agent_paths: fuyao_api::AgentPaths::default(),
        definition: fuyao_api::AgentDefinition::default(),
        session_params: Arc::new(tokio::sync::Mutex::new(test_session_params())),
        emitter,
        guide: empty_queue(),
        pending: empty_queue(),
        last_usage: Arc::new(tokio::sync::Mutex::new(None)),
        compression_config: fuyao_api::CompressionConfig::default(),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
        subagent_ops: None,
    };
    let mut session = Session {
        id: "test_session".to_string(),
        ..Session::default()
    };
    ctx.store.create(&session).await.unwrap();

    // 投两条消息进队列，注入后应都被拦截改写
    let msgs = vec![make_inbound("秘密1"), make_inbound("秘密2")];
    queue::inject_messages(&ctx, &mut session, msgs).await;

    // 验证：DB 里的 content 是拦截后的（带 [脱敏] 前缀）
    let visible: Vec<_> = ctx
        .store
        .load_visible_messages(&session.id, usize::MAX)
        .await
        .unwrap();
    assert_eq!(visible.len(), 2, "两条 user 消息应都进 DB");
    assert_eq!(
        visible[0].content.as_deref(),
        Some("[脱敏]秘密1"),
        "user 消息经拦截后内容应进 DB"
    );
    assert_eq!(
        visible[1].content.as_deref(),
        Some("[脱敏]秘密2"),
        "第二条 user 消息也应被拦截改写"
    );
}

/// 插件/系统来源的 source 字段完整流到 DB 的产出事件（消费时刻发 UI）
///
/// 验证修复错误①：source 字段不再丢失。检查 inject_messages 走 emit_to_history 后
/// 发出的事件携带原始 source（含 Plugin 名称）。
#[tokio::test]
async fn inject_messages_preserves_plugin_source_in_event() {
    let (tx_event, mut rx_event) = mpsc::unbounded_channel::<OutputEvent>();
    let emitter = Emitter::new(tx_event, "test_session".to_string());
    let hooks: fuyao_hooks::SharedHooks =
        Arc::new(tokio::sync::Mutex::new(HooksRegistry::default()));
    let store = temp_store().await;
    let ctx = SessionCtx {
        store,
        providers: Arc::new(fuyao_provider::ProviderRegistry::with_instance(
            "test",
            Arc::new(MockProvider::new(vec![])),
        )),
        tools: Arc::new(ToolRegistry::builder().build()),
        hooks,
        agent_paths: fuyao_api::AgentPaths::default(),
        definition: fuyao_api::AgentDefinition::default(),
        session_params: Arc::new(tokio::sync::Mutex::new(test_session_params())),
        emitter,
        guide: empty_queue(),
        pending: empty_queue(),
        last_usage: Arc::new(tokio::sync::Mutex::new(None)),
        compression_config: fuyao_api::CompressionConfig::default(),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
        subagent_ops: None,
    };
    let mut session = Session {
        id: "test_session".to_string(),
        ..Session::default()
    };
    ctx.store.create(&session).await.unwrap();

    // 构造一条 Plugin 来源消息（模拟 SessionSender.send_user 注入）
    let inbound = OutputUserMessage {
        base: EventBase::default(),
        payload: OutputUserPayload {
            content: "循环检测提醒".into(),
            images: vec![],
            mode: UserMessageMode::Guide,
            source: UserMessageSource::Plugin(fuyao_api::message::input::PluginSource {
                name: "loop_guard".into(),
            }),
        },
    };
    queue::inject_messages(&ctx, &mut session, vec![inbound]).await;

    // 收到的事件应是 OutputEvent::User 且 source = Plugin(loop_guard)
    let received = rx_event.try_recv().expect("应收到 User 事件");
    match received {
        OutputEvent::User(m) => match m.payload.source {
            UserMessageSource::Plugin(p) => assert_eq!(p.name, "loop_guard"),
            _ => panic!("source 应为 Plugin，实际：{:?}", m.payload.source),
        },
        _ => panic!("应为 User 事件"),
    }
}

/// 构造带图输出用户消息
fn make_inbound_with_images(content: &str) -> OutputUserMessage {
    OutputUserMessage {
        base: EventBase::default(),
        payload: OutputUserPayload {
            content: content.to_string(),
            images: vec![fuyao_api::ImageContent {
                mime_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            }],
            mode: UserMessageMode::Guide,
            source: UserMessageSource::User,
        },
    }
}

#[tokio::test]
async fn inject_images_omitted_when_model_unsupported() {
    // 模型未声明图片能力（默认安全）：图不落库，content 附加占位文本告警
    let provider = Arc::new(MockProvider::new(vec![]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    queue::inject_messages(
        &h.ctx,
        &mut h.session,
        vec![make_inbound_with_images("看图")],
    )
    .await;

    let visible = visible_messages(&h).await;
    assert_eq!(visible.len(), 1);
    assert!(visible[0].images.is_empty(), "不支持图时图片不应落库");
    let content = visible[0].content.as_deref().unwrap();
    assert!(content.starts_with("看图"), "文本应原样保留");
    assert!(
        content.contains("[图片已省略"),
        "content 应含图片省略占位文本，实际：{content}"
    );
}

#[tokio::test]
async fn inject_images_empty_text_uses_placeholder_only() {
    // 消息无文本只有图 + 模型不支持：content 就是占位文本本身
    let provider = Arc::new(MockProvider::new(vec![]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    queue::inject_messages(&h.ctx, &mut h.session, vec![make_inbound_with_images("")]).await;

    let visible = visible_messages(&h).await;
    assert_eq!(
        visible[0].content.as_deref(),
        Some("[图片已省略：当前模型不支持图像输入]")
    );
}

#[tokio::test]
async fn inject_images_persisted_when_model_supports() {
    // 模型声明输入模态含 image：图随消息完整落库，content 原样
    // 用独立 agent_id 的缓存键注册模型，避免污染默认键上其他并行测试
    let paths = fuyao_api::AgentPaths {
        agent_id: Some("test/images-support".into()),
        workspace: None,
        extra_dirs: vec![],
        fuyao_home: std::env::temp_dir().join("fuyao_core_test_home"),
    };
    let model = fuyao_api::Model {
        name: "test-model".into(),
        cost: Default::default(),
        limit: Default::default(),
        reasoning_efforts: vec![],
        modalities: fuyao_api::ModelModalities {
            input: vec![
                fuyao_api::InputModality::Text,
                fuyao_api::InputModality::Image,
            ],
            output: vec![fuyao_api::OutputModality::Text],
        },
    };
    let key = fuyao_provider::agent_paths_cache_key(&paths);
    fuyao_provider::register_model("test/test-model", model, &key);

    let provider = Arc::new(MockProvider::new(vec![]));
    let mut h = make_harness_full(
        provider,
        Arc::new(ToolRegistry::builder().build()),
        empty_hooks(),
        paths.clone(),
    )
    .await;
    queue::inject_messages(
        &h.ctx,
        &mut h.session,
        vec![make_inbound_with_images("看图")],
    )
    .await;

    let visible = visible_messages(&h).await;
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].images.len(), 1, "支持图时图片应完整落库");
    assert_eq!(visible[0].images[0].mime_type, "image/png");
    assert_eq!(visible[0].images[0].data, "aGVsbG8=");
    assert_eq!(visible[0].content.as_deref(), Some("看图"), "content 原样");
    assert!(
        !visible[0]
            .content
            .as_deref()
            .unwrap()
            .contains("[图片已省略")
    );

    // 清理独立缓存键（只影响本测试）
    fuyao_provider::clear_cache(&paths);
}

/// 手动压缩：直接调 run_manual_compression，验证跳过阈值 + reason=manual + 复用执行流程
///
/// 关键点：last_usage 仍为 None（从未跑过 turn），自动压缩会因此早退；
/// 手动压缩必须无视阈值直接执行，且 Started/Ended 的 reason 标记为 manual。
#[tokio::test]
async fn manual_compression_skips_threshold_and_marks_manual() {
    use fuyao_api::message::output::{CompressionPayload, CompressionReason};

    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response(
        "压缩摘要",
    )]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    // 预置多条可见消息（压缩对象）。手动压缩跳过阈值门，fallback_context 仅影响
    // CompressionStarted 事件里的 context_length 展示值，不影响压缩能否执行。
    preload_user(&mut h, "第一段对话内容").await;
    preload_user(&mut h, "第二段对话内容").await;
    preload_user(&mut h, "第三段对话内容").await;
    preload_user(&mut h, "第四段对话内容").await;
    // last_usage 为 None（harness 默认）——自动压缩会早退，手动压缩必须照常执行

    run_manual_compression(&h.ctx, &mut h.session).await;

    let events = collect_events(&mut h.rx_event).await;

    // Started：reason=manual
    let started = events.iter().find_map(|e| match e {
        OutputEvent::Compression(m) => match &m.payload {
            CompressionPayload::Started(p) => Some(p),
            _ => None,
        },
        _ => None,
    });
    let started = started.expect("应有 Compression Started 事件");
    assert_eq!(
        started.reason,
        CompressionReason::Manual,
        "手动压缩 reason 应为 manual"
    );

    // Ended：reason=manual + 摘要内容
    let ended = events.iter().find_map(|e| match e {
        OutputEvent::Compression(m) => match &m.payload {
            CompressionPayload::Ended(p) => Some(p),
            _ => None,
        },
        _ => None,
    });
    let ended = ended.expect("应有 Compression Ended 事件");
    assert_eq!(ended.reason, CompressionReason::Manual);
    assert_eq!(ended.content, "压缩摘要");
}

/// 回退命令经控制通道执行：删目标 seq 之后的消息 + 刷内存 session count + 发 Rollback 事件。
///
/// 验证阶段 1 的核心链路——handle_control 的 Rollback 分支：
/// 1. 调 store.rollback_to（删消息 + 重算）
/// 2. 就地刷新内存 session 的状态字段
/// 3. 经 dispatch 发 OutputEvent::Rollback 事件
///
/// 预置 user1(seq1) + assistant(seq2) + user2(seq3)，回退到 user1（target_seq=1），
/// 期待删掉 seq2/seq3、内存 message_count 刷新为 1、收到 Rollback 事件。
#[tokio::test]
async fn rollback_command_deletes_and_refreshes_session() {
    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response("ok")]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;

    // 预置三条消息：user1 → assistant → user2（insert 回填 seq，1/2/3）
    let mut u1 = fuyao_api::Message::user("第一条用户消息".to_string());
    h.ctx
        .store
        .insert_message(&h.session.id, &mut u1)
        .await
        .unwrap();
    let mut a1 = fuyao_api::Message::assistant(Some("助手回复".to_string()));
    h.ctx
        .store
        .insert_message(&h.session.id, &mut a1)
        .await
        .unwrap();
    let mut u2 = fuyao_api::Message::user("第二条用户消息".to_string());
    h.ctx
        .store
        .insert_message(&h.session.id, &mut u2)
        .await
        .unwrap();
    h.session.message_count = 2; // 两条 user（assistant 不计入 message_count 口径）
    h.session.tool_call_count = 0;

    // 执行回退命令：回到第一条 user 消息（target_seq = u1.seq）
    handle_control(
        &h.ctx,
        &mut h.session,
        ControlCommand::Rollback { target_seq: u1.seq },
    )
    .await;

    // 1. 收到 Rollback 事件，payload 字段符合预期
    let events = collect_events(&mut h.rx_event).await;
    let rollback = events
        .iter()
        .find_map(|e| match e {
            OutputEvent::Rollback(m) => Some(m),
            _ => None,
        })
        .expect("应收到 Rollback 事件");
    assert_eq!(
        rollback.payload.target_seq, u1.seq,
        "target_seq 应为回退目标"
    );
    assert_eq!(
        rollback.payload.deleted_total, 2,
        "应删掉 assistant + user2 共 2 条"
    );
    assert_eq!(
        rollback.payload.deleted_count, 1,
        "deleted_count 口径（user+compaction）应为 1（仅 user2）"
    );
    assert_eq!(
        rollback.payload.message_count, 1,
        "重算后 message_count 应为 1"
    );
    assert_eq!(rollback.base.session_id.as_deref(), Some("test_session"));

    // 2. 内存 session 的状态字段已就地刷新（避免后续 persist 盖回旧值）
    assert_eq!(h.session.message_count, 1, "内存 message_count 应已刷新");
    assert_eq!(h.session.tool_call_count, 0);
    assert_eq!(h.session.compression_count, 0);
    assert!(h.session.last_compacted_seq.is_none());

    // 3. DB 实际只剩 target 这一条
    let msgs = h.ctx.store.load_full_history(&h.session.id).await.unwrap();
    assert_eq!(msgs.len(), 1, "DB 应只剩目标消息");
    assert_eq!(msgs[0].seq, u1.seq);
    assert_eq!(msgs[0].role, fuyao_api::MessageRole::User);
}

/// 回退到非法目标（assistant 消息）：校验在 store 层原子完成，
/// 发 Error 事件、DB 不变、内存 session 不变。
#[tokio::test]
async fn rollback_to_invalid_target_emits_error_and_keeps_db() {
    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response("ok")]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;

    // user1(seq1) + assistant(seq2)
    let mut u1 = fuyao_api::Message::user("用户消息".to_string());
    h.ctx
        .store
        .insert_message(&h.session.id, &mut u1)
        .await
        .unwrap();
    let mut a1 = fuyao_api::Message::assistant(Some("助手回复".to_string()));
    h.ctx
        .store
        .insert_message(&h.session.id, &mut a1)
        .await
        .unwrap();
    h.session.message_count = 1;

    // 回退到 assistant（非法目标——中间态不可作回退点）
    handle_control(
        &h.ctx,
        &mut h.session,
        ControlCommand::Rollback { target_seq: a1.seq },
    )
    .await;

    // 收到 Error 事件（可恢复）
    let events = collect_events(&mut h.rx_event).await;
    let has_error = events
        .iter()
        .any(|e| matches!(e, OutputEvent::Error(m) if m.payload.recoverable));
    assert!(has_error, "非法目标应发可恢复的 Error 事件");

    // DB 不变（两条消息都在），内存 session 不变
    let msgs = h.ctx.store.load_full_history(&h.session.id).await.unwrap();
    assert_eq!(msgs.len(), 2, "非法回退不应改动 DB");
    assert_eq!(h.session.message_count, 1, "内存 session 不应变");
}

/// ReAct 间隙检查点：控制通道有待处理的 StopTurn 命令时，run_turn 在 loop 顶部
/// 立即捕获并执行，不调用 LLM 直接 return。
///
/// 验证：预置消息 + 预先投递 Rollback → run_turn 进 loop 顶部间隙检查 → 执行回退 →
/// return（无 Chunk / Assistant 事件，LLM 未被调用）+ 收到 Rollback 事件 + DB 已删消息。
#[tokio::test]
async fn react_gap_checkpoint_catches_rollback_before_llm() {
    // 即使 MockProvider 配了响应，间隙检查在 build_chat_request 之前，LLM 根本不会被调
    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response(
        "不该被调用",
    )]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;

    // 预置三条消息：user1(seq1) → assistant(seq2) → user2(seq3)
    let mut u1 = fuyao_api::Message::user("第一条用户消息".to_string());
    h.ctx
        .store
        .insert_message(&h.session.id, &mut u1)
        .await
        .unwrap();
    let mut a1 = fuyao_api::Message::assistant(Some("助手回复".to_string()));
    h.ctx
        .store
        .insert_message(&h.session.id, &mut a1)
        .await
        .unwrap();
    let mut u2 = fuyao_api::Message::user("第二条用户消息".to_string());
    h.ctx
        .store
        .insert_message(&h.session.id, &mut u2)
        .await
        .unwrap();
    h.session.message_count = 2;

    // 预先投递 Rollback（回到第一条 user 消息）到控制通道
    h.tx_control
        .send(ControlCommand::Rollback { target_seq: u1.seq })
        .await
        .unwrap();

    // run_turn 进 loop 顶部间隙检查点：捕获 Rollback → 执行 → persist → return
    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;

    let events = collect_events(&mut h.rx_event).await;

    // 收到 Rollback 事件
    let has_rollback = events
        .iter()
        .any(|e| matches!(e, OutputEvent::Rollback(m) if m.payload.target_seq == u1.seq));
    assert!(has_rollback, "间隙检查应执行回退并发出 Rollback 事件");

    // LLM 未被调用：无 Chunk / Assistant 事件
    let has_llm_output = events
        .iter()
        .any(|e| matches!(e, OutputEvent::Chunk(_) | OutputEvent::Assistant(_)));
    assert!(
        !has_llm_output,
        "间隙检查在 LLM 调用前 return，不应有任何 LLM 产出事件"
    );

    // DB 已删消息（只剩目标 seq1），内存 session 已刷新
    let msgs = h.ctx.store.load_full_history(&h.session.id).await.unwrap();
    assert_eq!(msgs.len(), 1, "回退应已删除目标 seq 之后的消息");
    assert_eq!(msgs[0].seq, u1.seq);
    assert_eq!(h.session.message_count, 1, "内存 session 应已刷新");
}

/// 间隙检查点 FIFO 忠实执行：多条命令按入队顺序逐条执行。
///
/// 投两条 Rollback（第一条回退到 seq1，第二条非法——seq1 已是最后一条，回退到它无后续可删
/// 但合法），验证两条都被执行（两次 Rollback 事件）、run_turn 退出。
#[tokio::test]
async fn react_gap_checkpoint_drains_multiple_commands_in_order() {
    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response(
        "不该被调用",
    )]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;

    // 预置 user1(seq1) + assistant(seq2) + user2(seq3)
    let mut u1 = fuyao_api::Message::user("用户1".to_string());
    h.ctx
        .store
        .insert_message(&h.session.id, &mut u1)
        .await
        .unwrap();
    let mut a1 = fuyao_api::Message::assistant(Some("回复".to_string()));
    h.ctx
        .store
        .insert_message(&h.session.id, &mut a1)
        .await
        .unwrap();
    let mut u2 = fuyao_api::Message::user("用户2".to_string());
    h.ctx
        .store
        .insert_message(&h.session.id, &mut u2)
        .await
        .unwrap();
    h.session.message_count = 2;

    // 投两条 Rollback：第一条回到 seq1（删 seq2/3），第二条回到 seq1（此时只剩 seq1，无后续可删，合法）
    h.tx_control
        .send(ControlCommand::Rollback { target_seq: u1.seq })
        .await
        .unwrap();
    h.tx_control
        .send(ControlCommand::Rollback { target_seq: u1.seq })
        .await
        .unwrap();

    turn::run_turn(
        &h.ctx,
        &mut h.session,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;

    let events = collect_events(&mut h.rx_event).await;
    // 两条命令都被执行：两次 Rollback 事件
    let rollback_count = events
        .iter()
        .filter(|e| matches!(e, OutputEvent::Rollback(_)))
        .count();
    assert_eq!(rollback_count, 2, "间隙检查应 FIFO 逐条执行所有待处理命令");

    // DB 最终只剩 seq1（第一条删了 seq2/3，第二条回退到 seq1 无后续可删）
    let msgs = h.ctx.store.load_full_history(&h.session.id).await.unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].seq, u1.seq);
}
