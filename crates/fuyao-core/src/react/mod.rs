//! ReAct 循环 task（session 执行流）
//!
//! 每个活跃 session 独立运行一个 task，消费 guide 队列，驱动 ReAct 循环：
//! 想（LLM）→ 可能调一批工具 → 全部工具完成后消费 guide → 再想 → ... → 最终回复。
//!
//! 并发模型：多 session 各自一个 task，tokio 调度，异步并发。
//! 同 session 内单 task 串行（处理完一个 turn 才取下一个）。
//!
//! 双队列（guide / pending）：
//! - guide：直接消费的队列，触发消费时机时一次性全部取出，每条变一条 user message
//!   注入 session.messages，回循环顶部调 LLM
//! - pending：排队队列，AI 不再调工具（最终回复）后才一次性全部倒进 guide
//!
//! 两个消费时机（详见 [`turn::run_turn`]）：
//! - 一批工具全部执行完成后、发回 AI 前：只看 guide（还在调工具，pending 不动）
//! - AI 不调用工具（最终回复，一轮 ReAct 结束）：先 pending 全倒 guide，再 guide 全消费
//!
//! 中断通道与队列分离：Interrupt 走独立 `rx_interrupt`（mpsc），
//! select! 中断点只监听它——不会误取 User/Plugin。
//!
//! 工具结果不走队列：它是 ReAct 循环内部中间产物，直接 push 进 session.messages。

mod builders;
pub(crate) mod queue;
pub(crate) mod turn;

use crate::emit::Emitter;
use crate::engine::types::SharedQueue;
use crate::interrupt::emit_interrupt_event;
use crate::tool_registry::ToolRegistry;
use fuyao_api::Session;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::input::InterruptMessage;
use fuyao_provider::Provider;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::sync::mpsc::{Receiver, Sender};

/// session 的共享依赖（引擎级共享能力的 owned 视图）
///
/// 聚合 store / provider / tools / agent_paths / emitter / guide / pending
/// 这些所有 turn 都需要的共享只读依赖 + 双队列，避免 run_turn 参数列表过长。
/// 不含可变状态（session / rx_interrupt）——那些作为独立 &mut 参数传入。
/// 由 run_session 构造一次，整个 task 期间以 `&SessionCtx` 不可变借用复用。
pub(crate) struct SessionCtx {
    pub store: Arc<fuyao_session::SessionStore>,
    pub provider: Arc<dyn Provider>,
    pub tools: Arc<ToolRegistry>,
    pub agent_paths: fuyao_api::AgentPaths,
    pub emitter: Emitter,
    /// 引导队列（直接消费）
    pub guide: SharedQueue,
    /// 排队队列（最终回复后转入 guide）
    pub pending: SharedQueue,
}

/// session 的独立执行流
///
/// 消费 guide 队列驱动 ReAct 循环；guide 空时 select! 等待 notify（新消息入队）
/// 或 rx_interrupt（idle 中断）。pending 入队也会 notify（覆盖 AI 空闲只发 Pending）。
///
/// 关于 SessionParams 的简化（有意决策）：`SessionParams` 在 `Engine::create_session`
/// 里被消费——只取出 `agent_config` 构建 system_prompt 存进 `Session.system_prompt`，
/// 之后 SessionParams 本身不再传入 task。task 运行时需要的配置走两条路：
/// - 工具配置：引擎级 `ToolRegistry` 共享（`tools` 参数），启动时装配。
/// - session 级配置（system_prompt）：已构建进 `Session`，task 直接读。
///
/// 这样多 session 并发时工具共享、人格隔离，互不干扰。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_session(
    session_id: String,
    guide: SharedQueue,
    pending: SharedQueue,
    notify: Arc<Notify>,
    mut rx_interrupt: Receiver<InterruptMessage>,
    mut session: Session,
    store: Arc<fuyao_session::SessionStore>,
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    agent_paths: fuyao_api::AgentPaths,
    tx_event: Sender<OutputEvent>,
) {
    tracing::info!(session_id = %session_id, "session 执行流启动");

    let ctx = SessionCtx {
        store,
        provider,
        tools,
        agent_paths,
        emitter: Emitter::new(tx_event, session_id.clone()),
        guide,
        pending,
    };

    // 主循环：从 guide 全取消息 → 注入 → 跑一轮 ReAct；guide 空 → 等待
    loop {
        let msgs = queue::consume_all_guide(&ctx.guide);
        if !msgs.is_empty() {
            // 取第一条消息的 params（决定本轮 model/options）
            // ReAct 多轮复用同一份 model（一个 turn 一个模型）
            // TODO: 多条 guide 消息 params 不一致时如何取——当前取第一条
            let first_params = msgs.first().map(|m| m.params.clone()).unwrap_or_default();
            // 一次性全部注入：每条变一条 user message
            queue::inject_messages(&ctx.emitter, &mut session, msgs).await;
            turn::run_turn(&ctx, &mut session, &mut rx_interrupt, first_params).await;
        } else {
            // guide 空：等 notify（新消息入队）或中断
            tokio::select! {
                // notify 唤醒：回循环顶部重新 consume（guide 或 pending 可能有新消息）
                () = notify.notified() => { continue; }
                Some(interrupt_msg) = rx_interrupt.recv() => {
                    // idle 中断：无活跃 turn，只发通知事件
                    emit_interrupt_event(&interrupt_msg.payload, &ctx.emitter).await;
                    tracing::debug!(session_id = %ctx.emitter.session_id(), "idle 时收到中断信号");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
        let session = Session::new(None, Some("系统提示词".to_string()));
        store.create(&session).await.unwrap();
        let (tx_event, rx_event) = mpsc::channel(128);
        let (tx_interrupt, rx_interrupt) = mpsc::channel(8);
        let ctx = SessionCtx {
            store,
            provider,
            tools,
            agent_paths: fuyao_api::AgentPaths::default(),
            emitter: Emitter::new(tx_event, "test_session".to_string()),
            guide: empty_queue(),
            pending: empty_queue(),
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
            OutputEvent::TurnStart(m) => m.base.session_id.as_deref(),
            OutputEvent::Chunk(m) => m.base.session_id.as_deref(),
            OutputEvent::User(m) => m.base.session_id.as_deref(),
            OutputEvent::ToolCall(m) => m.base.session_id.as_deref(),
            OutputEvent::ToolResult(m) => m.base.session_id.as_deref(),
            OutputEvent::Assistant(m) => m.base.session_id.as_deref(),
            OutputEvent::Interrupt(m) => m.base.session_id.as_deref(),
            OutputEvent::Error(m) => m.base.session_id.as_deref(),
            OutputEvent::Plugin(m) => m.base.session_id.as_deref(),
            OutputEvent::QueueUpdate(m) => m.base.session_id.as_deref(),
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
        let has_assistant = events.iter().any(|e| {
            matches!(e, OutputEvent::Assistant(m) if m.payload.content.as_deref() == Some("你好"))
        });
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
}
