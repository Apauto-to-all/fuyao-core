//! ReAct 循环单元测试
//!
//! 测试组织：MockProvider 驱动 + TestHarness 聚合共享依赖 + 临时 DB 隔离。
//! 覆盖面：run_turn 基本 ReAct 行为 / 双队列三消费时机（含 turn 内入站通道入队路径、
//! 最终回复后触发下一轮）/ 中断与 shutdown 收尾 / TurnOutcome 消费许可状态机 /
//! run_session 主循环（pending 空闲解禁、停止后重启、连发消息同 turn 批量消费）。

use super::*;
use async_trait::async_trait;
use futures_util::stream;
use fuyao_api::MessageRole;
use fuyao_api::Session;
use fuyao_api::message::EventBase;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::input::{UserMessageMode, UserMessageSource};
use fuyao_api::message::output::{
    InterruptMessage as OutputInterruptMessage, UserMessage as OutputUserMessage,
    UserPayload as OutputUserPayload,
};

/// 测试固定使用的 session_id（落库后内核不再常驻内存 Session，只认 DB + id）
const TEST_SESSION_ID: &str = "test_session";
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
        Err(StreamError::ApiError {
            status: None,
            message: "mock: chat 不支持".into(),
        })
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
        Err(StreamError::ApiError {
            status: None,
            message: "mock: chat 不支持".into(),
        })
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
        Box::pin(async move { fuyao_api::ToolOutput::text(format!("echo:{s}")) })
    });
    let entry = fuyao_api::ToolEntry {
        definition: fuyao_api::ToolDefinition::new("echo", "回显参数"),
        handler,
        child_invisible: false,
    };
    Arc::new(ToolRegistry::builder().register(entry).build())
}

/// 测试用 ModelConfig：携带 `test/...` 形式的 model_id
///
/// 必须带 model_id——model_id 为空时 turn.rs::resolve_model 返回 Err，
/// 走配置错误分支结束 turn，测试 ReAct 行为就跑不起来。所有测试的
/// ProviderRegistry 都用 `with_instance("test", ...)` 构造，model_id 拆出的
/// provider_id = "test" 能匹配到。
fn test_params() -> fuyao_api::ModelConfig {
    fuyao_api::ModelConfig {
        model_id: "test/test-model".to_string(),
        thinking_type: None,
        reasoning_effort: None,
    }
}

/// 测试用 SessionParams（model_id 同 test_params，供 SessionCtx 构造用）
fn test_session_params() -> fuyao_api::SessionParams {
    fuyao_api::SessionParams {
        agent_config: fuyao_api::AgentConfig {
            definition: "default".to_string(),
        },
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
    Arc::new(fuyao_hooks::HooksRegistry::default())
}

/// 构造测试用 SessionCtx + session + rx_interrupt + 收事件的 rx
///
/// `tx_interrupt` 仅供测试主动发中断用——turn 的中断分支是 `Some(...)` 模式，
/// 通道关闭只是禁用分支，无需为「保活」而持有 tx（drop 也不影响 turn 语义）。
struct TestHarness {
    ctx: SessionCtx,
    /// 本 harness 关联的 session_id（DB 唯一数据源，内核不再常驻内存 Session）
    session_id: String,
    /// 入站通道接收端：run_turn 流式 / 工具执行两段 select! 消费它
    rx_inbound: mpsc::Receiver<OutputUserMessage>,
    /// 入站通道发送端：测试向 turn 运行期间注入用户消息用（与生产 Engine::send 同路径）
    tx_inbound: mpsc::Sender<OutputUserMessage>,
    rx_interrupt: Receiver<OutputInterruptMessage>,
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

/// 构造测试用 SessionCtx 构造器（统一走生产同款 builder，消除字段级镜像）
///
/// 必填字段给测试默认（default 定义 / test 模型参数 / is_child=false），调用方按需
/// 链式覆盖可选字段（guide / pending / shutdown_token 等）后 build。
/// SessionCtx 增删字段时本工厂自动跟随 builder，各构造点不再各自维护字面量。
fn test_ctx_builder(
    store: Arc<fuyao_session::SessionStore>,
    providers: Arc<fuyao_provider::ProviderRegistry>,
    tools: Arc<ToolRegistry>,
    hooks: fuyao_hooks::SharedHooks,
    emitter: Emitter,
    agent_paths: fuyao_api::AgentPaths,
) -> SessionCtxBuilder {
    SessionCtx::builder(
        store,
        providers,
        tools,
        hooks,
        agent_paths,
        fuyao_api::AgentDefinition::default(),
        Arc::new(tokio::sync::Mutex::new(test_session_params())),
        emitter,
        false,
    )
}

/// 同 make_harness_with_hooks，但可指定 agent_paths
///
/// 全局模型缓存按 agent_paths 隔离——需要注册独立模型配置（如费用测试的
/// 价格表）的测试用独立的 agent_id 键，避免污染/被污染其他并行测试的缓存。
async fn make_harness_full(
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    hooks: fuyao_hooks::SharedHooks,
    agent_paths: fuyao_api::AgentPaths,
) -> TestHarness {
    let store = temp_store().await;
    // DB 唯一数据源：构造 Session 仅用于落库，落库后内核不再持有内存 Session，
    // 只凭 session_id 查 DB。此处 create 完即丢弃 Session 对象。
    let mut session = Session::new(None, None, Some("系统提示词".to_string()));
    // 强制 session.id 与 emitter 的 session_id 一致
    // （build_chat_request 用 emitter.session_id() 查 DB，必须匹配）
    session.id = TEST_SESSION_ID.to_string();
    store.create(&session).await.unwrap();
    drop(session);
    let (tx_event, rx_event) = mpsc::unbounded_channel();
    // 入站通道：与生产同容量（16），测试经 tx_inbound 模拟 Engine::send 的投递路径
    let (tx_inbound, rx_inbound) = mpsc::channel::<OutputUserMessage>(16);
    let (tx_interrupt, rx_interrupt) = mpsc::channel(8);
    // 控制通道：tx 保留供测试注入命令，rx 供 run_turn 间隙检查点 try_recv
    let (tx_control, rx_control) = mpsc::channel::<ControlCommand>(8);
    // 包成 ProviderRegistry：测试里所有 model_id 都用 "test/..."，统一走 test provider。
    // turn.rs 从 ctx.providers.get(provider_id) 取实例，必须找到才能继续。
    let providers = Arc::new(fuyao_provider::ProviderRegistry::with_instance(
        "test", provider,
    ));
    let ctx = test_ctx_builder(
        store,
        providers,
        tools,
        hooks,
        Emitter::new(tx_event, TEST_SESSION_ID.to_string()),
        agent_paths,
    )
    .build();
    TestHarness {
        ctx,
        session_id: TEST_SESSION_ID.to_string(),
        rx_inbound,
        tx_inbound,
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
        OutputEvent::Compression(m) => m.base.session_id.as_deref(),
        OutputEvent::Title(m) => m.base.session_id.as_deref(),
        OutputEvent::Retry(m) => m.base.session_id.as_deref(),
        OutputEvent::ChildSession(m) => m.base.session_id.as_deref(),
        OutputEvent::Rollback(m) => m.base.session_id.as_deref(),
    }
}

/// 预置一条 user 消息进 DB（模拟主循环 inject 后的状态）
///
/// 消息直接调 store.insert_message 落 DB——sessions 表的 message_count 由
/// insert_message 事务内原子累加，无需测试侧手动同步内存计数器。
async fn preload_user(h: &TestHarness, content: &str) {
    let mut msg = fuyao_api::Message::user(content.to_string());
    h.ctx
        .store
        .insert_message(&h.session_id, &mut msg)
        .await
        .expect("preload 落库失败");
}

/// 从 DB 加载可见消息（DB 唯一数据源，断言一律走 DB 查询）
async fn visible_messages(h: &TestHarness) -> Vec<fuyao_api::Message> {
    h.ctx
        .store
        .load_visible_messages(&h.session_id, usize::MAX)
        .await
        .expect("加载可见消息失败")
}

/// 无工具单轮：user → 流式回复 → AssistantMessage
#[tokio::test]
async fn single_turn_no_tools() {
    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response("你好")]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&h, "用户问题").await;

    turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
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
    preload_user(&h, "调工具").await;

    turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
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
    preload_user(&h, "test").await;

    turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
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
    preload_user(&h, "test").await;

    turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
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
    preload_user(&h, "原始问题").await;
    // 工具执行期间用户补充 2 条 guide 消息（模拟入队）
    h.ctx.guide.lock().unwrap().push_back(make_inbound("补充1"));
    h.ctx.guide.lock().unwrap().push_back(make_inbound("补充2"));

    turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
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
///
/// 第一轮直接最终回复（无工具），触发消费时机②：pending + guide 都被消费注入，
/// **并触发下一轮 ReAct 循环**（第二轮 LLM 调用消费 mock 的「回复2」）——
/// 修复前注入后 run_turn 直接 return Completed，消息进 DB 但 AI 永不回应。
#[tokio::test]
async fn pending_before_guide_on_final_reply() {
    // 第一轮直接最终回复（无工具），触发消费时机②；注入后再调一轮（「回复2」）
    let provider = Arc::new(MockProvider::new(vec![
        MockProvider::text_response("回复1"),
        // 注入 pending+guide 消息后再调一轮，最终回复
        MockProvider::text_response("回复2"),
    ]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&h, "第一条").await;
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

    let outcome = turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;

    // 两条都应进 DB（pos 闭包在消息缺失时直接 panic）
    let msgs = visible_messages(&h).await;
    let pos = |needle: &str| {
        msgs.iter()
            .position(|m| m.content.as_deref() == Some(needle))
            .unwrap_or_else(|| panic!("消息「{needle}」应进 DB"))
    };
    pos("排队消息");
    pos("引导消息");
    // 两个队列都空
    assert!(h.ctx.pending.lock().unwrap().is_empty());
    assert!(h.ctx.guide.lock().unwrap().is_empty());

    // 顺序语义：pending 追加在 guide 现有内容之后（引导消息在前、排队消息在后）
    assert!(pos("引导消息") < pos("排队消息"));

    // 消费注入后触发下一轮 ReAct：两条 user 消息都排在「回复1」之后、
    // 且 AI 对它们给出了「回复2」（同 turn 内被回应）
    assert!(pos("排队消息") > pos("回复1"));
    assert!(pos("引导消息") > pos("回复1"));
    assert!(pos("排队消息") < pos("回复2"));
    assert!(pos("引导消息") < pos("回复2"));
    // turn 正常结束（双队列跑空后 Completed）
    assert!(
        matches!(outcome, turn::TurnOutcome::Completed),
        "双队列跑空后应返回 Completed"
    );
}

/// guide + pending 都空，最终回复后 turn 结束（不追加额外消息）
#[tokio::test]
async fn both_empty_turn_ends() {
    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response("回复")]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&h, "问题").await;

    turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;

    // user + assistant，没有多余消息
    let msgs = visible_messages(&h).await;
    assert_eq!(msgs.len(), 2);
}

/// 工具执行期间经入站通道到达的 guide 消息，在消费时机①（整批工具完成后）被消费
///
/// 走生产同款投递路径（tx_inbound 通道，Engine::send 同路）：消息由工具执行段
/// select! 的 inbound 分支即时入队。修复前 turn 运行期间通道无人消费，消息滞留到
/// turn 结束后才入队——消费时机①在生产链路上永远消费到空队列。
///
/// 时序：阻塞工具 sleep 200ms 制造 exec_fut 的 Pending 窗口，测试在窗口内（约 50ms）
/// 投递消息——inbound 分支在工具批完成前即时入队，消费时机①必然看到它。
#[tokio::test]
async fn guide_via_channel_consumed_after_tool_batch() {
    // 阻塞工具：sleep 200ms 后完成（Pending 窗口，等测试在窗口内投递消息）
    let blocking_handler: fuyao_api::ToolFn = Arc::new(|_args, _ctx, _cancel| {
        Box::pin(async {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            fuyao_api::ToolOutput::text("done")
        })
    });
    let tools = ToolRegistry::builder()
        .register(fuyao_api::ToolEntry {
            definition: fuyao_api::ToolDefinition::new("blocking_tool", "阻塞窗口工具"),
            handler: blocking_handler,
            child_invisible: false,
        })
        .build();

    let provider = Arc::new(MockProvider::new(vec![
        MockProvider::tool_call_response("tc_1", "blocking_tool", r#"{}"#),
        MockProvider::text_response("最终回复"),
    ]));
    let mut h = make_harness(provider, Arc::new(tools)).await;
    preload_user(&h, "原始问题").await;

    let tx_inbound = h.tx_inbound.clone();
    let turn_fut = turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    );
    tokio::pin!(turn_fut);
    let driver = async {
        // 等 run_turn 跑完流式（工具调用）并进入工具执行的 Pending 窗口
        for _ in 0..6 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // 工具执行期间投 guide 消息（inbound 分支即时入队，不打断工具批）
        tx_inbound.send(make_inbound("工具期间补充")).await.unwrap();
    };
    let outcome = tokio::select! {
        _ = &mut turn_fut => panic!("工具阻塞 200ms，turn 不可能先于投递完成"),
        _ = driver => {
            tokio::time::timeout(std::time::Duration::from_secs(2), turn_fut)
                .await
                .expect("run_turn 应在工具完成后结束")
        }
    };
    assert!(
        matches!(outcome, turn::TurnOutcome::Completed),
        "双队列跑空后应返回 Completed，实际: {outcome:?}"
    );

    // DB 顺序：user → assistant(tool_calls) → tool → user(补充) → assistant(最终)
    // 直取 ctx.store（字段级借用）——turn_fut 的 PinMut 仍持有 h.rx_inbound 等可变借用
    let msgs = h
        .ctx
        .store
        .load_visible_messages(&h.session_id, usize::MAX)
        .await
        .expect("加载可见消息失败");
    let pos = |needle: &str| {
        msgs.iter()
            .position(|m| m.content.as_deref() == Some(needle))
            .unwrap_or_else(|| panic!("消息「{needle}」应进 DB"))
    };
    let tool_pos = msgs
        .iter()
        .position(|m| matches!(m.role, MessageRole::Tool))
        .expect("应有 tool 消息");
    assert!(
        tool_pos < pos("工具期间补充"),
        "补充消息应在工具结果之后注入（消费时机①），实际位置：tool={tool_pos}, 补充={}",
        pos("工具期间补充")
    );
    assert!(
        pos("工具期间补充") < pos("最终回复"),
        "补充消息应在最终回复之前（同 turn 内被 AI 看到）"
    );
    assert!(h.ctx.guide.lock().unwrap().is_empty(), "guide 应被消费空");
}

/// turn 启动前连投两条 guide 消息进通道：流式段 inbound 分支先于流结果轮询（biased），
/// 两条都赶在消费时机①入队，同一 turn 内一次性批量消费——而非各开独立 turn。
/// 修复前通道消息滞留，run_turn 全程消费不到，两条都不进 DB。
#[tokio::test]
async fn guide_via_channel_batched_in_one_turn() {
    let provider = Arc::new(MockProvider::new(vec![
        MockProvider::tool_call_response("c1", "echo", r#"{}"#),
        MockProvider::text_response("最终回复"),
    ]));
    let mut h = make_harness(provider, echo_registry()).await;
    preload_user(&h, "原始问题").await;
    // 连投两条（模拟用户快速连发）
    h.tx_inbound.send(make_inbound("补充1")).await.unwrap();
    h.tx_inbound.send(make_inbound("补充2")).await.unwrap();

    turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;

    // 恰 6 条：user / assistant(tool_calls) / tool / 补充1 / 补充2 / assistant(最终)
    let msgs = visible_messages(&h).await;
    assert_eq!(
        msgs.len(),
        6,
        "两条补充都应在本 turn 消费时机①注入，实际消息数: {msgs:?}"
    );
    let pos = |needle: &str| {
        msgs.iter()
            .position(|m| m.content.as_deref() == Some(needle))
            .unwrap_or_else(|| panic!("消息「{needle}」应进 DB"))
    };
    // 批量顺序保持 FIFO：补充1 在补充2 前，且都在工具结果后、最终回复前
    let tool_pos = msgs
        .iter()
        .position(|m| matches!(m.role, MessageRole::Tool))
        .expect("应有 tool 消息");
    assert!(tool_pos < pos("补充1"));
    assert!(pos("补充1") < pos("补充2"));
    assert!(pos("补充2") < pos("最终回复"));
    assert!(h.ctx.guide.lock().unwrap().is_empty());
}

/// 流式期间到达的 guide 消息：inbound 分支即时入队（不打断流式），最终回复后在
/// 消费时机②被消费，并触发下一轮 ReAct 循环（AI 对补充消息给出「第二轮回复」）
#[tokio::test]
async fn inbound_during_streaming_consumed_at_final_reply() {
    // 批 0：吐一个 TextDelta 后挂起（等测试投补充消息再喂 Done）；
    // 批 1：补充消息触发的下一轮，预喂完整最终回复后 drop 发送端（流读到关闭即结束）
    let (provider, mut txs) = ControllableProvider::with_batches(2);
    let provider: Arc<dyn Provider> = Arc::new(provider);
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&h, "原始问题").await;

    txs[0]
        .send(Ok(StreamEvent::TextDelta {
            content: "第一轮回复".to_string(),
        }))
        .unwrap();
    {
        let tx1 = txs.pop().unwrap();
        tx1.send(Ok(StreamEvent::TextDelta {
            content: "第二轮回复".to_string(),
        }))
        .unwrap();
        tx1.send(Ok(StreamEvent::Done {
            usage: StreamUsage::default(),
            finish_reason: FinishReason::Stop,
        }))
        .unwrap();
    }

    let tx_inbound = h.tx_inbound.clone();
    // 独占批 0 的发送端（pop 取走所有权）：喂完 Done 后 drop 关闭通道，
    // 流读到关闭即结束——clone 会留下第二个发送端，通道不关流不结束
    let tx0 = txs.pop().unwrap();
    let turn_fut = turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    );
    tokio::pin!(turn_fut);
    let driver = async {
        // 等 run_turn 消费掉 TextDelta 并挂起在流的第二个事件上
        for _ in 0..6 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // 流式挂起期间投 guide 消息（inbound 分支即时入队，不打断流式）
        tx_inbound.send(make_inbound("流式期间补充")).await.unwrap();
        // 补充消息入队后喂 Done 并 drop 发送端，流结束 → 最终回复 → 消费时机②
        tx0.send(Ok(StreamEvent::Done {
            usage: StreamUsage::default(),
            finish_reason: FinishReason::Stop,
        }))
        .unwrap();
        drop(tx0);
    };
    let outcome = tokio::select! {
        _ = &mut turn_fut => unreachable!("流挂起时 turn 不可能先完成"),
        _ = driver => {
            tokio::time::timeout(std::time::Duration::from_secs(2), turn_fut)
                .await
                .expect("run_turn 应在两轮 ReAct 后结束")
        }
    };
    assert!(
        matches!(outcome, turn::TurnOutcome::Completed),
        "双队列跑空后应返回 Completed"
    );

    // DB 顺序：user → assistant(第一轮) → user(补充) → assistant(第二轮)
    // 直取 ctx.store（字段级借用）——turn_fut 的 PinMut 仍持有 h.rx_inbound 等可变借用
    let msgs = h
        .ctx
        .store
        .load_visible_messages(&h.session_id, usize::MAX)
        .await
        .expect("加载可见消息失败");
    let pos = |needle: &str| {
        msgs.iter()
            .position(|m| m.content.as_deref() == Some(needle))
            .unwrap_or_else(|| panic!("消息「{needle}」应进 DB"))
    };
    assert!(pos("原始问题") < pos("第一轮回复"));
    assert!(
        pos("第一轮回复") < pos("流式期间补充"),
        "补充消息应在第一轮最终回复之后注入（消费时机②）"
    );
    assert!(
        pos("流式期间补充") < pos("第二轮回复"),
        "补充消息应触发下一轮 ReAct 并被 AI 回应"
    );
}

/// 中断语义（turn 内入队路径）：流式期间入队的 guide 消息，中断后不被消费、
/// 不被清除，原样保留在队列——直到下一条用户消息恢复消费（TurnOutcome 状态机）
#[tokio::test]
async fn interrupt_after_inbound_preserves_guide_queue() {
    use fuyao_api::InterruptSource;
    use fuyao_api::message::output::InterruptMessage;

    let (provider, txs) = ControllableProvider::with_batches(1);
    let provider: Arc<dyn Provider> = Arc::new(provider);
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&h, "原始问题").await;

    // 吐一个 TextDelta 后挂起（流等第二个事件）
    txs[0]
        .send(Ok(StreamEvent::TextDelta {
            content: "部分回复".to_string(),
        }))
        .unwrap();

    let tx_inbound = h.tx_inbound.clone();
    let tx_interrupt = h.tx_interrupt.clone();
    let turn_fut = turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    );
    tokio::pin!(turn_fut);
    let driver = async {
        for _ in 0..6 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // 流式挂起期间投 guide 消息（inbound 分支即时入队）
        tx_inbound.send(make_inbound("中断前补充")).await.unwrap();
        // 留时间让入队发生（select! 下一轮轮询即入队），再发中断
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        tx_interrupt
            .send(InterruptMessage::new("用户取消", InterruptSource::User))
            .await
            .unwrap();
    };
    let outcome = tokio::select! {
        _ = &mut turn_fut => unreachable!("流挂起 + 无 Done 时 turn 不可能先完成"),
        _ = driver => {
            tokio::time::timeout(std::time::Duration::from_secs(2), turn_fut)
                .await
                .expect("run_turn 应在中断后结束")
        }
    };
    assert!(
        matches!(outcome, turn::TurnOutcome::Interrupted),
        "流式期间中断应返回 Interrupted"
    );

    // 队列保留：guide 恰含这条消息，未被消费、未被清除
    let guide_contents: Vec<String> = h
        .ctx
        .guide
        .lock()
        .unwrap()
        .iter()
        .map(|m| m.payload.content.clone())
        .collect();
    assert_eq!(
        guide_contents,
        vec!["中断前补充".to_string()],
        "中断后 guide 剩余应原样保留（不消费、不清除）"
    );
    // 未消费 = 未过历史入口落库（直取 ctx.store——turn_fut 仍持有字段级可变借用）
    let msgs = h
        .ctx
        .store
        .load_visible_messages(&h.session_id, usize::MAX)
        .await
        .expect("加载可见消息失败");
    let users: Vec<String> = msgs
        .iter()
        .filter(|m| matches!(m.role, MessageRole::User))
        .map(|m| m.content.clone().unwrap_or_default())
        .collect();
    assert!(
        !users.contains(&"中断前补充".to_string()),
        "中断后未消费的消息不应落 DB"
    );
}

/// turn 运行期间经入站通道到达的 pending 消息：消费时机①不消费（pending 不动），
/// 最终回复后在消费时机②被消费并触发下一轮 ReAct——「等链结束」语义在
/// turn 内入队路径下依然成立
#[tokio::test]
async fn pending_via_channel_consumed_at_final_reply_not_tool_batch() {
    // 三轮 LLM：工具调用 → 第一轮最终回复（时机①不动 pending）→
    // 时机②消费 pending 触发的下一轮 → 第二轮最终回复
    let provider = Arc::new(MockProvider::new(vec![
        MockProvider::tool_call_response("tc_1", "echo", r#"{}"#),
        MockProvider::text_response("第一轮回复"),
        MockProvider::text_response("第二轮回复"),
    ]));
    let mut h = make_harness(provider, echo_registry()).await;
    preload_user(&h, "原始问题").await;
    // turn 启动前投一条 pending 消息（流式段 inbound 分支入队，走生产同款通道路径）
    h.tx_inbound
        .send(make_inbound_with_mode("排队补充", UserMessageMode::Pending))
        .await
        .unwrap();

    turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;

    let msgs = visible_messages(&h).await;
    let pos = |needle: &str| {
        msgs.iter()
            .position(|m| m.content.as_deref() == Some(needle))
            .unwrap_or_else(|| panic!("消息「{needle}」应进 DB"))
    };
    // 排队补充不在消费时机①注入（否则会排在「第一轮回复」之前），
    // 而是在时机②注入（第一轮回复之后）并被下一轮回应（第二轮回复之前）
    assert!(
        pos("第一轮回复") < pos("排队补充"),
        "pending 不应在工具批完成时消费（时机①不动 pending）"
    );
    assert!(
        pos("排队补充") < pos("第二轮回复"),
        "pending 应在时机②消费并触发下一轮 ReAct 被回应"
    );
    assert!(h.ctx.pending.lock().unwrap().is_empty());
    assert!(h.ctx.guide.lock().unwrap().is_empty());
}

/// TurnOutcome 消费许可状态机：四态许可判定 + 任意非 Completed 态经新用户消息恢复
///
/// 守护主循环「停消费 / 恢复消费」的全部语义迁移——历史上最贵的队列死信 bug
/// 都藏在这些迁移上，语义表在此钉死。
#[test]
fn turn_outcome_consumption_state_machine() {
    // 许可判定：仅 Completed 允许消费 guide/pending
    assert!(turn::TurnOutcome::Completed.may_consume());
    assert!(!turn::TurnOutcome::HaltedByCommand.may_consume());
    assert!(!turn::TurnOutcome::Interrupted.may_consume());
    assert!(!turn::TurnOutcome::Failed.may_consume());

    // 恢复迁移：任意非 Completed 态经新用户消息恢复为可消费
    for outcome in [
        turn::TurnOutcome::HaltedByCommand,
        turn::TurnOutcome::Interrupted,
        turn::TurnOutcome::Failed,
    ] {
        let mut o = outcome;
        o.resume_on_new_intent();
        assert!(o.may_consume(), "{outcome:?} 恢复后应允许消费");
    }

    // Completed 自恢复无变化
    let mut completed = turn::TurnOutcome::Completed;
    completed.resume_on_new_intent();
    assert!(completed.may_consume());
}

/// 中断通道关闭不影响 turn 正常执行（Some 模式：关闭 = 分支禁用，不当作事件）
///
/// 关闭后流继续等 Done、最终回复正常产出、不产生任何 Interrupt 事件——
/// 调用方无需为保证通道打开而持有 tx（旧语义下此处会 false 中断 / 忙循环）。
#[tokio::test]
async fn closed_interrupt_channel_does_not_disturb_turn() {
    let (provider, txs) = ControllableProvider::with_batches(1);
    let provider: Arc<dyn Provider> = Arc::new(provider);
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&h, "test").await;

    // 预喂一个 TextDelta：流启动后挂起在第二个事件上
    txs[0]
        .send(Ok(StreamEvent::TextDelta {
            content: "你好".to_string(),
        }))
        .unwrap();
    // 关闭中断通道（drop 全部 tx）
    drop(h.tx_interrupt);
    // 50ms 后补 Done——期间通道已关闭，若关闭被当作事件处理（旧语义），
    // 流式段会立即「中断/continue」，正常收尾不可能发生
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = txs[0].send(Ok(StreamEvent::Done {
            usage: StreamUsage::default(),
            finish_reason: FinishReason::Stop,
        }));
    });

    let outcome = turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;
    assert!(
        matches!(outcome, turn::TurnOutcome::Completed),
        "通道关闭不应打断 turn，实际退出原因: {outcome:?}"
    );

    let events = collect_events(&mut h.rx_event).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutputEvent::Assistant(m) if m.payload.content.as_deref() == Some("你好"))),
        "应正常产出最终回复"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutputEvent::Interrupt(_))),
        "通道关闭不应模拟出 Interrupt 事件"
    );
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
    // 中断 / 控制通道：tx 直接 drop 即关闭——select! 的 Some 模式下关闭 = 分支禁用，
    // 无需保活（通道关闭语义的活验证：session 只靠 inbound / shutdown 驱动）
    let (_tx_interrupt, rx_interrupt) = mpsc::channel::<OutputInterruptMessage>(8);
    let (_tx_control, rx_control) = mpsc::channel::<ControlCommand>(8);
    let (tx_event, mut rx_event) = mpsc::unbounded_channel();

    // 启动 session 执行流（两队列都空，task 进入 select! 等待）
    let providers = Arc::new(fuyao_provider::ProviderRegistry::with_instance(
        "test", provider,
    ));
    let ctx = test_ctx_builder(
        Arc::clone(&store),
        providers,
        Arc::new(ToolRegistry::builder().build()),
        empty_hooks(),
        Emitter::new(tx_event, "test_session".to_string()),
        fuyao_api::AgentPaths::default(),
    )
    .guide(Arc::clone(&guide))
    .pending(Arc::clone(&pending))
    .shutdown_token(tokio_util::sync::CancellationToken::new())
    .build();
    let task = tokio::spawn(run_session(
        ctx,
        SessionRx {
            inbound: rx_inbound,
            interrupt: rx_interrupt,
            control: rx_control,
        },
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
    // 中断 / 控制通道：tx 直接 drop 即关闭（关闭 = 分支禁用，无需保活）
    let (_tx_interrupt, rx_interrupt) = mpsc::channel::<OutputInterruptMessage>(8);
    let (_tx_control, rx_control) = mpsc::channel::<ControlCommand>(8);
    let (tx_event, mut rx_event) = mpsc::unbounded_channel();

    let providers = Arc::new(fuyao_provider::ProviderRegistry::with_instance(
        "test", provider,
    ));
    let ctx = test_ctx_builder(
        Arc::clone(&store),
        providers,
        Arc::new(ToolRegistry::builder().build()),
        empty_hooks(),
        Emitter::new(tx_event, "test_session".to_string()),
        fuyao_api::AgentPaths::default(),
    )
    .guide(Arc::clone(&guide))
    .pending(Arc::clone(&pending))
    .shutdown_token(tokio_util::sync::CancellationToken::new())
    .build();
    let task = tokio::spawn(run_session(
        ctx,
        SessionRx {
            inbound: rx_inbound,
            interrupt: rx_interrupt,
            control: rx_control,
        },
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

/// 快速连发两条消息：第二条在第一条 turn 的流式期间经 inbound 分支入队，
/// 最终回复后在消费时机②被消费——同一个 turn 内被 AI 回应（回复1 → 回复2），
/// 而非各开独立 turn。修复前第二条滞留通道，第一条 turn 结束后才各开新 turn。
#[tokio::test]
async fn rapid_fire_messages_answered_in_single_turn() {
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
    let (_tx_interrupt, rx_interrupt) = mpsc::channel::<OutputInterruptMessage>(8);
    let (_tx_control, rx_control) = mpsc::channel::<ControlCommand>(8);
    let (tx_event, mut rx_event) = mpsc::unbounded_channel();

    let providers = Arc::new(fuyao_provider::ProviderRegistry::with_instance(
        "test", provider,
    ));
    let ctx = test_ctx_builder(
        Arc::clone(&store),
        providers,
        Arc::new(ToolRegistry::builder().build()),
        empty_hooks(),
        Emitter::new(tx_event, "test_session".to_string()),
        fuyao_api::AgentPaths::default(),
    )
    .guide(Arc::clone(&guide))
    .pending(Arc::clone(&pending))
    .shutdown_token(tokio_util::sync::CancellationToken::new())
    .build();
    let task = tokio::spawn(run_session(
        ctx,
        SessionRx {
            inbound: rx_inbound,
            interrupt: rx_interrupt,
            control: rx_control,
        },
    ));

    // 连投两条（都进通道后 task 才开始消费——第二条必然在第一条 turn 期间被
    // 流式段的 inbound 分支入队，赶上前面的消费点）
    tx_inbound.send(make_inbound("问题1")).await.unwrap();
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
        "连发的两条消息都应在同一 turn 内被回应（回复1 → 回复2）"
    );

    // DB 顺序：user(问题1) → assistant(回复1) → user(问题2) → assistant(回复2)
    let msgs = store
        .load_visible_messages("test_session", usize::MAX)
        .await
        .unwrap();
    let pos = |needle: &str| {
        msgs.iter()
            .position(|m| m.content.as_deref() == Some(needle))
            .unwrap_or_else(|| panic!("消息「{needle}」应进 DB"))
    };
    assert!(pos("问题1") < pos("回复1"));
    assert!(
        pos("回复1") < pos("问题2"),
        "问题2 应在第一轮最终回复后注入（消费时机②），而非抢先落库"
    );
    assert!(pos("问题2") < pos("回复2"));
    assert!(guide.lock().unwrap().is_empty());
    assert!(pending.lock().unwrap().is_empty());
}

/// 中断退出保留 guide 剩余；新用户消息恢复消费，旧剩余 + 新消息一起跑
///
/// 验证 TurnOutcome 消费许可状态机最贵的迁移链：
/// 中断 → Interrupted（guide 剩余不消费、原样保留）→ 新 inbound 恢复许可 →
/// 一起消费跑完。历史上「非 Completed 剩余被误消费 / 恢复后丢消息」都藏在这条链上。
#[tokio::test]
async fn interrupted_turn_preserves_guide_until_new_inbound() {
    use fuyao_api::InterruptSource;
    use fuyao_api::message::output::InterruptMessage;

    // 批 0：吐一个 TextDelta 后挂起（等中断）；批 1：恢复后的最终回复（预喂进 unbounded 通道缓冲）
    let (provider, mut txs) = ControllableProvider::with_batches(2);
    let provider: Arc<dyn Provider> = Arc::new(provider);

    let store = temp_store().await;
    let mut session = Session::new(None, None, Some("系统提示词".to_string()));
    session.id = "test_session".to_string();
    store.create(&session).await.unwrap();

    let guide = empty_queue();
    let (tx_inbound, rx_inbound) = mpsc::channel::<OutputUserMessage>(16);
    // 中断通道：本测试要发中断，tx 保留发送用（不再是为保活）
    let (tx_interrupt, rx_interrupt) = mpsc::channel::<OutputInterruptMessage>(8);
    let (_tx_control, rx_control) = mpsc::channel::<ControlCommand>(8);
    let (tx_event, mut rx_event) = mpsc::unbounded_channel();

    let providers = Arc::new(fuyao_provider::ProviderRegistry::with_instance(
        "test", provider,
    ));
    let ctx = test_ctx_builder(
        Arc::clone(&store),
        providers,
        Arc::new(ToolRegistry::builder().build()),
        empty_hooks(),
        Emitter::new(tx_event, "test_session".to_string()),
        fuyao_api::AgentPaths::default(),
    )
    .guide(Arc::clone(&guide))
    .build();
    let task = tokio::spawn(run_session(
        ctx,
        SessionRx {
            inbound: rx_inbound,
            interrupt: rx_interrupt,
            control: rx_control,
        },
    ));

    txs[0]
        .send(Ok(StreamEvent::TextDelta {
            content: "你好".to_string(),
        }))
        .unwrap();
    // 批 1（恢复后的最终回复）：预喂完立即 drop 发送端——流读到通道关闭即结束，
    // turn 2 才能正常收尾（txs[0] 保留，让 turn 1 的流挂在第二个事件上等中断）
    {
        let tx1 = txs.pop().unwrap();
        tx1.send(Ok(StreamEvent::TextDelta {
            content: "ok".to_string(),
        }))
        .unwrap();
        tx1.send(Ok(StreamEvent::Done {
            usage: StreamUsage::default(),
            finish_reason: FinishReason::Stop,
        }))
        .unwrap();
    }

    // 第一条消息 → turn 1（流挂起在批 0 的第二个事件上）
    tx_inbound.send(make_inbound("问题1")).await.unwrap();
    let got_chunk = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(ev) = rx_event.recv().await {
            if matches!(ev, OutputEvent::Chunk(m) if m.payload.content.as_deref() == Some("你好"))
            {
                return true;
            }
        }
        false
    })
    .await;
    assert!(
        got_chunk.unwrap_or(false),
        "turn 1 应消费到 TextDelta 并挂起在流上"
    );

    // 中断 → turn 1 以 Interrupted 结束
    tx_interrupt
        .send(InterruptMessage::new("用户取消", InterruptSource::User))
        .await
        .unwrap();
    let got_interrupt = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(ev) = rx_event.recv().await {
            if matches!(ev, OutputEvent::Interrupt(_)) {
                return true;
            }
        }
        false
    })
    .await;
    assert!(got_interrupt.unwrap_or(false), "应收到 Interrupt 事件");
    // 等 run_session 回到 idle select（turn 返回后），再直塞一条 guide 剩余
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    guide.lock().unwrap().push_back(make_inbound("保留消息B"));

    // Interrupted 期间 guide 剩余不被消费（主循环跳过 consume，落 select! 等待）
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(
        !guide.lock().unwrap().is_empty(),
        "Interrupted 后 guide 剩余应原样保留，不被消费"
    );

    // 新用户消息 → 恢复许可 → 「旧剩余 B + 新消息 C」一起消费跑 turn 2
    tx_inbound.send(make_inbound("新消息C")).await.unwrap();
    let got_final = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(ev) = rx_event.recv().await {
            if matches!(ev, OutputEvent::Assistant(m) if m.payload.content.as_deref() == Some("ok"))
            {
                return true;
            }
        }
        false
    })
    .await;

    task.abort();

    assert!(
        got_final.unwrap_or(false),
        "恢复消费后应跑完 turn 2 并收到最终回复「ok」"
    );
    assert!(guide.lock().unwrap().is_empty(), "恢复消费后 guide 应跑空");

    // B 与 C 都应进 DB（旧剩余 + 新消息一起跑，不丢）
    let msgs = store
        .load_visible_messages("test_session", usize::MAX)
        .await
        .unwrap();
    let users: Vec<_> = msgs
        .iter()
        .filter(|m| matches!(m.role, MessageRole::User))
        .map(|m| m.content.clone().unwrap_or_default())
        .collect();
    assert!(
        users.contains(&"保留消息B".to_string()),
        "旧剩余应被消费进 DB"
    );
    assert!(
        users.contains(&"新消息C".to_string()),
        "新消息应被消费进 DB"
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
    preload_user(&h, "test").await;

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
        &mut h.rx_inbound,
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

    // 部分结果经统一历史入口落 DB：assistant 行携带 finish_reason=interrupted 与累积文本；
    // 中断补发走不计费路径（token 全 0、不填 model_id）。
    // 直取 ctx.store（字段级借用）——turn_fut 的 PinMut 仍持有 h.rx_interrupt 的可变借用
    let visible: Vec<_> = h
        .ctx
        .store
        .load_visible_messages(&h.session_id, usize::MAX)
        .await
        .unwrap();
    let interrupted_row = visible
        .iter()
        .find(|m| m.finish_reason.as_deref() == Some("interrupted"))
        .expect("中断补发的 assistant 消息应落 DB");
    assert_eq!(interrupted_row.content.as_deref(), Some("你好"));
    assert_eq!(interrupted_row.model_id, None);
    assert_eq!(interrupted_row.cost, 0.0);
}

/// 中断-流式期间（纯思考）：模型仅产出思考增量、正文未开始时中断 →
/// 落库的 assistant 行 content 补空串（content 与 tool_calls 双空违反
/// OpenAI 协议，会让下轮请求 400），reasoning 原样保留
#[tokio::test]
async fn interrupt_during_streaming_reasoning_only() {
    use fuyao_api::InterruptSource;
    use fuyao_api::message::output::InterruptMessage;

    // 准备 1 轮事件流（中断发生在首轮流式期间）
    let (provider, txs) = ControllableProvider::with_batches(1);
    let provider: Arc<dyn Provider> = Arc::new(provider);
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&h, "test").await;

    let tx_interrupt = h.tx_interrupt.clone();

    // 只喂思考增量（正文一个字未吐），流随后挂起在第二个事件上
    txs[0]
        .send(Ok(StreamEvent::ReasoningDelta {
            content: "先想一想".to_string(),
        }))
        .unwrap();

    let turn_fut = turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    );
    tokio::pin!(turn_fut);
    let interrupter = async {
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
                .expect("run_turn 应在中断后结束");
        }
    }

    // 落库行：content 为空串（双空被历史入口兜底）、reasoning 携带已累积思考
    let visible: Vec<_> = h
        .ctx
        .store
        .load_visible_messages(&h.session_id, usize::MAX)
        .await
        .unwrap();
    let interrupted_row = visible
        .iter()
        .find(|m| m.finish_reason.as_deref() == Some("interrupted"))
        .expect("中断补发的 assistant 消息应落 DB");
    assert_eq!(
        interrupted_row.content.as_deref(),
        Some(""),
        "纯思考中断的落库行 content 应为空串，而非 None"
    );
    assert_eq!(interrupted_row.reasoning.as_deref(), Some("先想一想"));
    assert!(interrupted_row.tool_calls.is_none());
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
            fuyao_api::ToolOutput::text("unreachable")
        })
    });
    let tools = ToolRegistry::builder()
        .register(fuyao_api::ToolEntry {
            definition: fuyao_api::ToolDefinition::new("blocking_tool", "阻塞测试工具"),
            handler: blocking_handler,
            child_invisible: false,
        })
        .build();

    let mut h = make_harness(provider, Arc::new(tools)).await;
    preload_user(&h, "调工具").await;

    let tx_interrupt = h.tx_interrupt.clone();

    let turn_fut = turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
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

/// 中断-工具执行期间（差集补发）：快工具已完成、阻塞工具未完成 → 只为未完成的中断补发
///
/// 未完成判定用本批内存观测的已答集：已完成工具不得重复补发中断式 ToolResult——
/// 否则同一 tool_call_id 出现两条结果，下轮 LLM 调用协议报错。
#[tokio::test]
async fn interrupt_during_tool_execution_only_completes_unfinished() {
    use fuyao_api::InterruptSource;
    use fuyao_api::message::output::InterruptMessage;

    // 一批两个工具调用：fast 立即完成（结果先被记录进已答集），slow 阻塞等中断
    let provider = Arc::new(MockProvider::new(vec![vec![
        Ok(StreamEvent::ToolCallChunk {
            index: 0,
            id: Some("tc_fast".to_string()),
            name: Some("fast_tool".to_string()),
            args_delta: Some("{}".to_string()),
        }),
        Ok(StreamEvent::ToolCallChunk {
            index: 1,
            id: Some("tc_slow".to_string()),
            name: Some("blocking_tool".to_string()),
            args_delta: Some("{}".to_string()),
        }),
        Ok(StreamEvent::Done {
            usage: StreamUsage::default(),
            finish_reason: FinishReason::ToolCalls,
        }),
    ]]));

    let fast_handler: fuyao_api::ToolFn =
        Arc::new(|_args, _ctx, _cancel| Box::pin(async { fuyao_api::ToolOutput::text("fast ok") }));
    let blocking_handler: fuyao_api::ToolFn = Arc::new(|_args, _ctx, _cancel| {
        Box::pin(async {
            // 永不完成：sleep 30 秒，足够测试发中断
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            fuyao_api::ToolOutput::text("unreachable")
        })
    });
    let tools = ToolRegistry::builder()
        .register(fuyao_api::ToolEntry {
            definition: fuyao_api::ToolDefinition::new("fast_tool", "立即完成的工具"),
            handler: fast_handler,
            child_invisible: false,
        })
        .register(fuyao_api::ToolEntry {
            definition: fuyao_api::ToolDefinition::new("blocking_tool", "阻塞测试工具"),
            handler: blocking_handler,
            child_invisible: false,
        })
        .build();

    let mut h = make_harness(provider, Arc::new(tools)).await;
    preload_user(&h, "调工具").await;

    let tx_interrupt = h.tx_interrupt.clone();

    let turn_fut = turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    );
    tokio::pin!(turn_fut);
    let interrupter = async {
        // 等 run_turn 进入工具执行段且 fast_tool 的结果已被记录进已答集
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutputEvent::Interrupt(_))),
        "应有 Interrupt 事件"
    );

    // fast：恰一条真实结果，无中断式补发（已答不重复补发）
    let fast_results: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            OutputEvent::ToolResult(m) if m.payload.tool_call_id == "tc_fast" => Some(&m.payload),
            _ => None,
        })
        .collect();
    assert_eq!(fast_results.len(), 1, "fast_tool 应恰一条 ToolResult");
    assert_eq!(fast_results[0].content, "fast ok");

    // slow：恰一条中断式 ToolResult（content 格式 [{source:?}][{reason}]）
    let slow_results: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            OutputEvent::ToolResult(m) if m.payload.tool_call_id == "tc_slow" => Some(&m.payload),
            _ => None,
        })
        .collect();
    assert_eq!(
        slow_results.len(),
        1,
        "blocking_tool 应恰一条中断式 ToolResult"
    );
    assert!(
        slow_results[0].content.contains("用户取消"),
        "中断式 ToolResult content 应含中断原因: {}",
        slow_results[0].content
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
    preload_user(&h, "test").await;

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
        &mut h.rx_inbound,
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
            fuyao_api::ToolOutput::text("unreachable")
        })
    });
    let tools = ToolRegistry::builder()
        .register(fuyao_api::ToolEntry {
            definition: fuyao_api::ToolDefinition::new("blocking_tool", "阻塞测试工具"),
            handler: blocking_handler,
            child_invisible: false,
        })
        .build();

    let mut h = make_harness(provider, Arc::new(tools)).await;
    preload_user(&h, "调工具").await;

    let shutdown_token = h.ctx.shutdown_token.clone();

    let turn_fut = turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
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
    preload_user(&h, "调工具").await;
    let session_id = h.session_id.clone();

    turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
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
    preload_user(&h, "提问").await;

    turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
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
    preload_user(&h, "测费用累积").await;

    // 用带 model_id 的 model_config，让累积逻辑能查到价格表
    let mut params = test_params();
    params.model_id = "test/cost-model".to_string();

    turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        params,
    )
    .await;

    // 清理全局缓存（避免污染后续测试）
    fuyao_provider::clear_cache(&agent_paths);

    // 从 DB 重新读取 session 行验证总计字段（DB 唯一数据源，
    // token/cost 由 insert_message 事务内原子累加进 sessions 表）
    let db_session = h
        .ctx
        .store
        .get(&h.session_id)
        .await
        .expect("get 不应失败")
        .expect("session 应已落库");

    // === 验证 1：session 总计正确累积（两轮相加） ===
    assert_eq!(
        db_session.total_prompt_tokens, 300,
        "工具调用轮(100) + 最终回复轮(200) = 300"
    );
    assert_eq!(
        db_session.total_completion_tokens, 130,
        "工具调用轮(50) + 最终回复轮(80) = 130"
    );
    assert_eq!(
        db_session.total_reasoning_tokens, 30,
        "工具调用轮(10) + 最终回复轮(20) = 30"
    );
    assert_eq!(
        db_session.total_cached_tokens, 60,
        "工具调用轮(20) + 最终回复轮(40) = 60"
    );

    // === 验证 2：cost 为非零（价格表已注入，按 /M 算） ===
    assert!(
        db_session.total_cost > 0.0,
        "注入价格表后 session.total_cost 应非零，实际 = {}",
        db_session.total_cost
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
    let diff = (sum_costs - db_session.total_cost).abs();
    assert!(
        diff < 1e-9,
        "两条 Message.cost({sum_costs}) 之和应等于 session.total_cost({})",
        db_session.total_cost
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
    let mut reg = HooksRegistry::default();
    reg.register_output_intercept(
        0,
        Arc::new(|ev: &OutputEvent| {
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
    reg.finalize();
    let hooks: fuyao_hooks::SharedHooks = Arc::new(reg);

    let mut h = make_harness_with_hooks(provider, tools, hooks).await;
    preload_user(&h, "用户问题").await;

    turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
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
    let request =
        super::builders::build_chat_request(h.ctx.store.as_ref(), &h.session_id, usize::MAX).await;
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

    let mut reg = HooksRegistry::default();
    reg.register_output_intercept(
        0,
        Arc::new(|ev: &OutputEvent| {
            if matches!(ev, OutputEvent::Assistant(_)) {
                InterceptResult::Block("拦截 assistant".to_string())
            } else {
                InterceptResult::Pass(ev.clone())
            }
        }),
    );
    reg.finalize();
    let hooks: fuyao_hooks::SharedHooks = Arc::new(reg);

    let mut h = make_harness_with_hooks(provider, tools, hooks).await;
    preload_user(&h, "用户问题").await;

    turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
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
    // total_cost 也应为 0（拦截 Block 的消息不计费）——从 DB 读 session 验证
    let db_session = h
        .ctx
        .store
        .get(&h.session_id)
        .await
        .expect("get 不应失败")
        .expect("session 应已落库");
    assert_eq!(db_session.total_cost, 0.0, "Block 时不应累积任何费用");
}

/// 用户消息经 inject_user_messages 时走统一历史入口：插件可在**消费时刻**拦截改写。
///
/// 验证拦截/落库/发送三时机对齐在消费时刻，与 assistant / tool_result 完全对称。
/// 修复前注入是裸 push，插件无法在 user 消息进历史时介入（拦截裂缝）。
#[tokio::test]
async fn inject_messages_intercepts_user_at_consume_time() {
    let (tx_event, _rx_event) = mpsc::unbounded_channel::<OutputEvent>();
    let emitter = Emitter::new(tx_event, "test_session".to_string());
    let mut reg = HooksRegistry::default();
    reg.register_output_intercept(
        0,
        Arc::new(|ev: &OutputEvent| {
            if let OutputEvent::User(m) = ev {
                let mut modified = m.clone();
                modified.payload.content = format!("[脱敏]{}", modified.payload.content);
                InterceptResult::Pass(OutputEvent::User(modified))
            } else {
                InterceptResult::Pass(ev.clone())
            }
        }),
    );
    reg.finalize();
    let hooks: fuyao_hooks::SharedHooks = Arc::new(reg);
    let store = temp_store().await;
    let ctx = test_ctx_builder(
        store,
        Arc::new(fuyao_provider::ProviderRegistry::with_instance(
            "test",
            Arc::new(MockProvider::new(vec![])),
        )),
        Arc::new(ToolRegistry::builder().build()),
        hooks,
        emitter,
        fuyao_api::AgentPaths::default(),
    )
    .build();
    // DB 唯一数据源：构造 Session 仅用于落库，之后只凭 session_id 查 DB
    let session = Session {
        id: "test_session".to_string(),
        ..Session::default()
    };
    let session_id = session.id.clone();
    ctx.store.create(&session).await.unwrap();

    // 投两条消息进队列，注入后应都被拦截改写
    let msgs = vec![make_inbound("秘密1"), make_inbound("秘密2")];
    crate::history::inject_user_messages(&ctx, msgs).await;

    // 验证：DB 里的 content 是拦截后的（带 [脱敏] 前缀）
    let visible: Vec<_> = ctx
        .store
        .load_visible_messages(&session_id, usize::MAX)
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
/// 验证修复错误①：source 字段不再丢失。检查 inject_user_messages 走统一历史入口后
/// 发出的事件携带原始 source（含 Plugin 名称）。
#[tokio::test]
async fn inject_messages_preserves_plugin_source_in_event() {
    let (tx_event, mut rx_event) = mpsc::unbounded_channel::<OutputEvent>();
    let emitter = Emitter::new(tx_event, "test_session".to_string());
    let hooks: fuyao_hooks::SharedHooks = Arc::new(HooksRegistry::default());
    let store = temp_store().await;
    let ctx = test_ctx_builder(
        store,
        Arc::new(fuyao_provider::ProviderRegistry::with_instance(
            "test",
            Arc::new(MockProvider::new(vec![])),
        )),
        Arc::new(ToolRegistry::builder().build()),
        hooks,
        emitter,
        fuyao_api::AgentPaths::default(),
    )
    .build();
    // DB 唯一数据源：构造 Session 仅用于落库，之后只凭 session_id 查 DB
    let session = Session {
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
    crate::history::inject_user_messages(&ctx, vec![inbound]).await;

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
///
/// `data` 传裸 base64（小图，字节达标，节流原样保留）
fn make_inbound_with_images(content: &str) -> OutputUserMessage {
    make_inbound_with_image_data(content, "aGVsbG8=".to_string())
}

/// 构造带任意图数据的输出用户消息（超标 / 非法数据用，验证节流失败分支）
fn make_inbound_with_image_data(content: &str, data: String) -> OutputUserMessage {
    OutputUserMessage {
        base: EventBase::default(),
        payload: OutputUserPayload {
            content: content.to_string(),
            images: vec![fuyao_api::ImageContent {
                mime_type: "image/png".into(),
                data,
            }],
            mode: UserMessageMode::Guide,
            source: UserMessageSource::User,
        },
    }
}

#[tokio::test]
async fn inject_images_persisted_faithfully() {
    // 图片忠实落库：不做任何模型能力判断，图随消息完整落库、content 原样。
    // harness 不注册任何模型（注册表为空）——落库路径不查模型注册表
    let provider = Arc::new(MockProvider::new(vec![]));
    let h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    crate::history::inject_user_messages(&h.ctx, vec![make_inbound_with_images("看图")]).await;

    let visible = visible_messages(&h).await;
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].images.len(), 1, "图片应完整落库");
    assert_eq!(visible[0].images[0].mime_type, "image/png");
    assert_eq!(visible[0].images[0].data, "aGVsbG8=");
    assert_eq!(visible[0].content.as_deref(), Some("看图"), "content 原样");
}

#[tokio::test]
async fn inject_images_failed_processing_appends_placeholder() {
    // 图片处理失败（超限且非法 base64，解码必败）：失败图省略，content 附加占位文本
    // ——处理失败容错分支，与模型能力无关。
    // 节流结果预写进事件：UI 收到的 User 事件与 DB 落库内容一致（同源同构）
    let bad_image = "!!!not-base64!!!".repeat(1_000_000); // 约 16MB，远超 5MB 上限
    let provider = Arc::new(MockProvider::new(vec![]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    crate::history::inject_user_messages(
        &h.ctx,
        vec![make_inbound_with_image_data("看图", bad_image)],
    )
    .await;

    let visible = visible_messages(&h).await;
    assert_eq!(visible.len(), 1);
    assert!(visible[0].images.is_empty(), "处理失败的图不应落库");
    let content = visible[0].content.as_deref().unwrap();
    assert!(content.starts_with("看图"), "文本应原样保留");
    assert!(
        content.contains("[图片已省略：图片处理失败]"),
        "content 应含处理失败占位文本，实际：{content}"
    );

    // UI 侧事件与 DB 同一份：content 含占位文本、images 为空（预写进事件的结果）
    let events = collect_events(&mut h.rx_event).await;
    let user_event = events
        .iter()
        .find_map(|e| match e {
            OutputEvent::User(m) => Some(m),
            _ => None,
        })
        .expect("应发出 User 事件");
    assert!(
        user_event
            .payload
            .content
            .contains("[图片已省略：图片处理失败]"),
        "UI 事件 content 应与 DB 一致（含占位文本），实际：{}",
        user_event.payload.content
    );
    assert!(
        user_event.payload.images.is_empty(),
        "UI 事件不应携带处理失败的图"
    );
}

#[tokio::test]
async fn inject_images_failed_processing_empty_text_placeholder_only() {
    // 无文本 + 处理失败：占位文本即全文
    let bad_image = "!!!not-base64!!!".repeat(1_000_000);
    let provider = Arc::new(MockProvider::new(vec![]));
    let h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    crate::history::inject_user_messages(&h.ctx, vec![make_inbound_with_image_data("", bad_image)])
        .await;

    let visible = visible_messages(&h).await;
    assert_eq!(
        visible[0].content.as_deref(),
        Some("[图片已省略：图片处理失败]")
    );
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
    // 预置多条可见消息（压缩对象）。手动压缩跳过阈值门——harness 未注册模型，
    // context_length 解析为 None 仅影响 CompressionStarted 事件里的展示值与
    // keep 预算，不影响压缩能否执行。
    preload_user(&h, "第一段对话内容").await;
    preload_user(&h, "第二段对话内容").await;
    preload_user(&h, "第三段对话内容").await;
    preload_user(&h, "第四段对话内容").await;
    // last_usage 为 None（harness 默认）——自动压缩会早退，手动压缩必须照常执行

    super::compression::run_manual_compression(&h.ctx).await;

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

/// 回退命令经控制通道执行：删目标 seq 之后的消息 + 重算 session count + 发 Rollback 事件。
///
/// 验证 handle_control 的 Rollback 分支：
/// 1. 调 store.rollback_to（删消息 + 单事务内重算 count 类字段并局部 UPDATE sessions 表）
/// 2. 经 dispatch 发 OutputEvent::Rollback 事件
///
/// DB 唯一数据源——重算结果直接写进 sessions 表，下轮读路径从 DB 取即最新值。
///
/// 预置 user1(seq1) + assistant(seq2) + user2(seq3)，回退到 user1（target_seq=1），
/// 期待删掉 seq2/seq3、DB session 的 message_count 重算为 1、收到 Rollback 事件。
#[tokio::test]
async fn rollback_command_deletes_and_refreshes_session() {
    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response("ok")]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;

    // 预置三条消息：user1 → assistant → user2（insert 回填 seq，1/2/3）
    let mut u1 = fuyao_api::Message::user("第一条用户消息".to_string());
    h.ctx
        .store
        .insert_message(&h.session_id, &mut u1)
        .await
        .unwrap();
    let mut a1 = fuyao_api::Message::assistant(Some("助手回复".to_string()));
    h.ctx
        .store
        .insert_message(&h.session_id, &mut a1)
        .await
        .unwrap();
    let mut u2 = fuyao_api::Message::user("第二条用户消息".to_string());
    h.ctx
        .store
        .insert_message(&h.session_id, &mut u2)
        .await
        .unwrap();

    // 执行回退命令：回到第一条 user 消息（target_seq = u1.seq）
    handle_control(&h.ctx, ControlCommand::Rollback { target_seq: u1.seq }).await;

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

    // 2. DB session 行的 count 类字段已被 rollback_to 事务内重算并写回
    let db_session = h
        .ctx
        .store
        .get(&h.session_id)
        .await
        .expect("get 不应失败")
        .expect("session 应已落库");
    assert_eq!(db_session.message_count, 1, "DB message_count 应已重算");
    assert_eq!(db_session.tool_call_count, 0);
    assert_eq!(db_session.compression_count, 0);
    assert!(db_session.last_compacted_seq.is_none());

    // 3. DB 实际只剩 target 这一条
    let msgs = h.ctx.store.load_full_history(&h.session_id).await.unwrap();
    assert_eq!(msgs.len(), 1, "DB 应只剩目标消息");
    assert_eq!(msgs[0].seq, u1.seq);
    assert_eq!(msgs[0].role, fuyao_api::MessageRole::User);
}

/// 回退到非法目标（assistant 消息）：校验在 store 层原子完成，
/// 发 Error 事件、DB 不变、DB session 行不变。
#[tokio::test]
async fn rollback_to_invalid_target_emits_error_and_keeps_db() {
    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response("ok")]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;

    // user1(seq1) + assistant(seq2)
    let mut u1 = fuyao_api::Message::user("用户消息".to_string());
    h.ctx
        .store
        .insert_message(&h.session_id, &mut u1)
        .await
        .unwrap();
    let mut a1 = fuyao_api::Message::assistant(Some("助手回复".to_string()));
    h.ctx
        .store
        .insert_message(&h.session_id, &mut a1)
        .await
        .unwrap();
    // insert_message 事务内已累加 message_count，落库后从 DB 读作基线
    let db_before = h
        .ctx
        .store
        .get(&h.session_id)
        .await
        .expect("get 不应失败")
        .expect("session 应已落库");

    // 回退到 assistant（非法目标——中间态不可作回退点）
    handle_control(&h.ctx, ControlCommand::Rollback { target_seq: a1.seq }).await;

    // 收到 Error 事件（可恢复）
    let events = collect_events(&mut h.rx_event).await;
    let has_error = events
        .iter()
        .any(|e| matches!(e, OutputEvent::Error(m) if m.payload.recoverable));
    assert!(has_error, "非法目标应发可恢复的 Error 事件");

    // DB 不变（两条消息都在），DB session 行不变
    let msgs = h.ctx.store.load_full_history(&h.session_id).await.unwrap();
    assert_eq!(msgs.len(), 2, "非法回退不应改动 DB");
    let db_after = h
        .ctx
        .store
        .get(&h.session_id)
        .await
        .expect("get 不应失败")
        .expect("session 应已落库");
    assert_eq!(
        db_after.message_count, db_before.message_count,
        "DB session message_count 不应变"
    );
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
        .insert_message(&h.session_id, &mut u1)
        .await
        .unwrap();
    let mut a1 = fuyao_api::Message::assistant(Some("助手回复".to_string()));
    h.ctx
        .store
        .insert_message(&h.session_id, &mut a1)
        .await
        .unwrap();
    let mut u2 = fuyao_api::Message::user("第二条用户消息".to_string());
    h.ctx
        .store
        .insert_message(&h.session_id, &mut u2)
        .await
        .unwrap();

    // 预先投递 Rollback（回到第一条 user 消息）到控制通道
    h.tx_control
        .send(ControlCommand::Rollback { target_seq: u1.seq })
        .await
        .unwrap();

    // run_turn 进 loop 顶部间隙检查点：捕获 Rollback → 执行 → return HaltedByCommand
    let outcome = turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;
    assert!(
        matches!(outcome, turn::TurnOutcome::HaltedByCommand),
        "间隙检查 StopTurn 命令应让 run_turn 返回 HaltedByCommand"
    );

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

    // DB 已删消息（只剩目标 seq1），DB session 行 count 已被 rollback_to 事务重算
    let msgs = h.ctx.store.load_full_history(&h.session_id).await.unwrap();
    assert_eq!(msgs.len(), 1, "回退应已删除目标 seq 之后的消息");
    assert_eq!(msgs[0].seq, u1.seq);
    let db_session = h
        .ctx
        .store
        .get(&h.session_id)
        .await
        .expect("get 不应失败")
        .expect("session 应已落库");
    assert_eq!(db_session.message_count, 1, "DB session 应已重算");
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
        .insert_message(&h.session_id, &mut u1)
        .await
        .unwrap();
    let mut a1 = fuyao_api::Message::assistant(Some("回复".to_string()));
    h.ctx
        .store
        .insert_message(&h.session_id, &mut a1)
        .await
        .unwrap();
    let mut u2 = fuyao_api::Message::user("用户2".to_string());
    h.ctx
        .store
        .insert_message(&h.session_id, &mut u2)
        .await
        .unwrap();

    // 投两条 Rollback：第一条回到 seq1（删 seq2/3），第二条回到 seq1（此时只剩 seq1，无后续可删，合法）
    h.tx_control
        .send(ControlCommand::Rollback { target_seq: u1.seq })
        .await
        .unwrap();
    h.tx_control
        .send(ControlCommand::Rollback { target_seq: u1.seq })
        .await
        .unwrap();

    let outcome = turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;
    assert!(
        matches!(outcome, turn::TurnOutcome::HaltedByCommand),
        "含 StopTurn 命令的批次应让 run_turn 返回 HaltedByCommand"
    );

    let events = collect_events(&mut h.rx_event).await;
    // 两条命令都被执行：两次 Rollback 事件
    let rollback_count = events
        .iter()
        .filter(|e| matches!(e, OutputEvent::Rollback(_)))
        .count();
    assert_eq!(rollback_count, 2, "间隙检查应 FIFO 逐条执行所有待处理命令");

    // DB 最终只剩 seq1（第一条删了 seq2/3，第二条回退到 seq1 无后续可删）
    let msgs = h.ctx.store.load_full_history(&h.session_id).await.unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].seq, u1.seq);
}

/// run_turn 正常完成（AI 给最终回复，无工具调用）应返回 `Completed`。
///
/// 这是主循环消费许可的「绿灯」——只有 Completed 才允许下一轮 consume。
#[tokio::test]
async fn run_turn_returns_completed_on_final_reply() {
    let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response("完成")]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;

    // 注入一条 user 消息驱动一轮 ReAct（无工具 → 最终回复 → 结束）
    let mut u = fuyao_api::Message::user("你好".to_string());
    h.ctx
        .store
        .insert_message(&h.session_id, &mut u)
        .await
        .unwrap();

    let outcome = turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;

    assert!(
        matches!(outcome, turn::TurnOutcome::Completed),
        "AI 给最终回复、双队列空，run_turn 应返回 Completed"
    );
}

/// run_turn 在 LLM 调用失败（不可恢复）时应返回 `Failed`。
///
/// 主循环据此停消费——失败后不该继续跑队列剩余消息。
#[tokio::test]
async fn run_turn_returns_failed_on_llm_error() {
    // AuthError 经 retry 耗尽后冒泡为不可恢复错误（与 llm_error_emits_error_event 同源）
    let provider = Arc::new(MockProvider::new(vec![vec![Err(StreamError::AuthError(
        "无效密钥".into(),
    ))]]));
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&h, "test").await;

    let outcome = turn::run_turn(
        &h.ctx,
        &mut h.rx_inbound,
        &mut h.rx_interrupt,
        &mut h.rx_control,
        test_params(),
    )
    .await;

    assert!(
        matches!(outcome, turn::TurnOutcome::Failed),
        "LLM 调用失败应让 run_turn 返回 Failed"
    );
}

// ===== 标题生成判定门测试 =====

/// chat 可用的 Provider：标题生成走非流式 chat，返回固定标题
struct TitleProvider;

#[async_trait]
impl Provider for TitleProvider {
    fn stream_chat(
        &self,
        _request: fuyao_provider::ChatRequest,
        _model: &str,
        _options: fuyao_provider::StreamOptions,
    ) -> BoxStream<Result<StreamEvent, StreamError>> {
        // 标题路径不调流式；返回空流兜底
        Box::pin(stream::iter(vec![]))
    }

    async fn chat(
        &self,
        _request: fuyao_provider::ChatRequest,
        _model: &str,
        _options: fuyao_provider::StreamOptions,
    ) -> Result<ChatResponse, StreamError> {
        Ok(ChatResponse {
            content: Some("测试标题".to_string()),
            reasoning: None,
            tool_calls: None,
            usage: StreamUsage::default(),
            finish_reason: FinishReason::Stop,
        })
    }
}

/// 首轮注入触发标题生成；判定门每 session 只开一次，第二次调用零成本跳过
#[tokio::test]
async fn title_spawns_once_on_first_injection() {
    let provider: Arc<dyn Provider> = Arc::new(TitleProvider);
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&h, "第一个问题").await;

    // 首次调用：首轮判定通过（DB 恰 1 条 user）→ spawn 标题生成
    super::title::maybe_spawn_title(&h.ctx, Some("第一个问题")).await;
    assert!(
        h.ctx.title_gate.load(std::sync::atomic::Ordering::Relaxed),
        "首次判定应消耗判定门"
    );

    // 等 Title 事件（fire-and-forget task 落库 + 发事件）
    let title = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(ev) = h.rx_event.recv().await {
            if let OutputEvent::Title(m) = ev {
                return Some(m.payload.title);
            }
        }
        None
    })
    .await;
    assert_eq!(
        title.expect("2 秒内应收到 Title 事件").as_deref(),
        Some("测试标题"),
        "标题事件应携带 chat 返回的标题"
    );

    // 第二次调用：判定门已消耗，不再触发（无新 Title 事件）
    super::title::maybe_spawn_title(&h.ctx, Some("第二个问题")).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !matches!(h.rx_event.try_recv(), Ok(OutputEvent::Title(_))),
        "判定门消耗后二次调用不应再发 Title 事件"
    );
}

/// 非首轮会话（user 数 > 1）不触发标题生成，判定门仍被消耗（首次判定即终局）
#[tokio::test]
async fn title_skipped_for_non_first_session() {
    let provider: Arc<dyn Provider> = Arc::new(TitleProvider);
    let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;
    preload_user(&h, "第一问").await;
    preload_user(&h, "第二问").await;

    super::title::maybe_spawn_title(&h.ctx, Some("第二问")).await;
    assert!(
        h.ctx.title_gate.load(std::sync::atomic::Ordering::Relaxed),
        "首次判定（即使跳过）也应消耗判定门"
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !matches!(h.rx_event.try_recv(), Ok(OutputEvent::Title(_))),
        "user 数 != 1 不应触发标题生成"
    );
}

/// 子 session（is_child=true 且默认 skip_child=true）豁免标题生成
#[tokio::test]
async fn title_skipped_for_child_session() {
    let provider: Arc<dyn Provider> = Arc::new(TitleProvider);
    // harness 只为借它的 session 落库流程；豁免判定在 provider 使用前短路，注册表内容无关紧要
    let h = make_harness(
        Arc::clone(&provider),
        Arc::new(ToolRegistry::builder().build()),
    )
    .await;
    preload_user(&h, "子会话首问").await;

    // 子 session 上下文：is_child 必填字段直接经 builder 入口传入（生产同款构造）
    let providers = Arc::new(fuyao_provider::ProviderRegistry::with_instance(
        "test", provider,
    ));
    let (tx_event, mut rx_event) = mpsc::unbounded_channel::<OutputEvent>();
    let ctx = SessionCtx::builder(
        h.ctx.store.clone(),
        providers,
        Arc::new(ToolRegistry::builder().build()),
        empty_hooks(),
        fuyao_api::AgentPaths::default(),
        fuyao_api::AgentDefinition::default(),
        Arc::new(tokio::sync::Mutex::new(test_session_params())),
        Emitter::new(tx_event, TEST_SESSION_ID.to_string()),
        true,
    )
    .build();

    super::title::maybe_spawn_title(&ctx, Some("子会话首问")).await;
    assert!(
        ctx.title_gate.load(std::sync::atomic::Ordering::Relaxed),
        "豁免判定也应消耗判定门"
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !matches!(rx_event.try_recv(), Ok(OutputEvent::Title(_))),
        "子 session 默认豁免，不应触发标题生成"
    );
}
