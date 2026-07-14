//! ReAct 循环 task
//!
//! 每个活跃 session 独立运行一个 task，消费消息队列，驱动 ReAct 循环：
//! 想（LLM）→ 可能调工具 → 执行工具 → 拿结果 → 再想 → ... → 最终回复。
//!
//! 并发模型：多 session 各自一个 task，tokio 调度，异步并发。
//! 同 session 内单 task 串行（从队列取一条、处理完、再取下一条）。
//!
//! 工具结果不走队列：它是 ReAct 循环内部中间产物，直接 push 进 task 本地的
//! `session.messages`，然后 continue 回循环顶部——下一轮 build_chat_request
//! 自然会把工具结果带上。
//!
//! 双队列（guide / pending）：
//! - guide：直接消费的消息（User 的 Guide 模式），驱动 ReAct 循环
//! - pending：工具执行期间用户补充的消息暂存，AI 不再调工具（最终回复完成）后才转入 guide
//!
//! 中断通道与数据通道分离：Interrupt 走独立 `rx_interrupt`，
//! select! 中断点只监听它——不会误取 User/Plugin。

use crate::emit::Emitter;
use crate::engine::types::QueuedMessage;
use crate::interrupt::{
    SharedTurnState, TurnState, classify, emit_interrupt_event, handle_interrupt,
};
use crate::stream::{self, StreamResult};
use crate::tool_exec;
use crate::tool_registry::ToolRegistry;
use fuyao_api::message::EventBase;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::input::InterruptMessage;
use fuyao_api::message::output::{
    AssistantMessage, AssistantPayload, ToolCallPayload, UserMessage as OutputUserMessage,
};
use fuyao_api::{Message, MessageParams, Session, UserMessageMode};
use fuyao_provider::{ChatMessage, ChatRequest, Provider, StreamDecoder, StreamOptions};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender};

/// session 的共享依赖（引擎级共享能力的 owned 视图）
///
/// 聚合 store / provider / tools / agent_paths / emitter 这些所有 turn 都需要的
/// 共享只读依赖，避免 run_turn / drain_pending 参数列表过长。
/// 不含可变状态（session / rx_interrupt / pending）——那些作为独立 &mut 参数传入。
/// 由 run_session 构造一次，整个 task 期间以 `&SessionCtx` 不可变借用复用。
pub(crate) struct SessionCtx {
    pub store: Arc<fuyao_session::SessionStore>,
    pub provider: Arc<dyn Provider>,
    pub tools: Arc<ToolRegistry>,
    pub agent_paths: fuyao_api::AgentPaths,
    pub emitter: Emitter,
}

/// session 的独立执行流
///
/// 消费 `rx_queue`（数据通道：User/Plugin），同时监听 `rx_interrupt`（中断通道）。
/// 数据通道关闭（所有 Sender drop）时退出。
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
    mut rx_queue: Receiver<QueuedMessage>,
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
    };

    // pending 队列：工具执行期间用户补充的消息暂存，AI 最终回复后转入处理
    let mut pending: VecDeque<(String, MessageParams)> = VecDeque::new();

    // 主循环：从数据队列取消息 → 处理 → 取下一条
    while let Some(msg) = rx_queue.recv().await {
        match msg.event {
            fuyao_api::InputEvent::User(user_msg) => {
                let mode = user_msg.payload.mode;
                let content = user_msg.payload.content.clone();
                let params = msg.params.unwrap_or_default();

                match mode {
                    UserMessageMode::Guide => {
                        // Guide 模式：直接跑一轮 ReAct
                        run_turn(&ctx, &mut session, &mut rx_interrupt, content, params).await;
                        // 本轮结束后，drain pending 队列（工具执行期间暂存的消息逐条处理）
                        drain_pending(&ctx, &mut session, &mut rx_interrupt, &mut pending).await;
                    }
                    UserMessageMode::Pending => {
                        // Pending 模式：暂存，等 AI 不再调工具后才处理
                        tracing::debug!(
                            session_id = ctx.emitter.session_id(),
                            pending_count = pending.len(),
                            "Pending 消息暂存"
                        );
                        pending.push_back((content, params));
                    }
                }
            }
            fuyao_api::InputEvent::Interrupt(_) => {
                // Interrupt 应走 rx_interrupt，数据通道不应收到——防御性记日志
                tracing::warn!(
                    session_id = ctx.emitter.session_id(),
                    "数据通道收到 Interrupt（应走中断通道）"
                );
            }
            fuyao_api::InputEvent::Plugin(plugin_msg) => {
                // 插件通知：转发为 OutputEvent::Plugin（input → output payload 重组）
                ctx.emitter
                    .emit(OutputEvent::Plugin(
                        fuyao_api::message::output::PluginMessage {
                            base: plugin_msg.base,
                            payload: fuyao_api::message::output::PluginPayload {
                                source: plugin_msg.payload.source,
                                event_type: plugin_msg.payload.event_type,
                                data: plugin_msg.payload.data,
                                error: plugin_msg.payload.error,
                                message: plugin_msg.payload.message,
                            },
                        },
                    ))
                    .await;
            }
        }
    }

    tracing::info!(session_id = ctx.emitter.session_id(), "session 执行流结束");
}

/// 处理 pending 队列：把暂存的消息逐条作为 Guide 跑一轮 turn
///
/// 最终回复完成后调，把工具执行期间用户补充的消息转入处理。
/// 每条 pending 消息独立跑一轮 turn（不再产生工具调用时自然结束）。
async fn drain_pending(
    ctx: &SessionCtx,
    session: &mut Session,
    rx_interrupt: &mut Receiver<InterruptMessage>,
    pending: &mut VecDeque<(String, MessageParams)>,
) {
    while let Some((content, params)) = pending.pop_front() {
        run_turn(ctx, session, rx_interrupt, content, params).await;
    }
}

/// 单轮 ReAct 循环
///
/// User 消息进历史 → 调 LLM → 有工具调用则执行工具、结果进历史、再调 LLM →
/// 无工具调用则发最终 AssistantMessage、落库。
///
/// 中断：两段 select!——流式期间、工具执行期间。idle 段在 run_session 外层
/// （主循环 rx_queue.recv() 等待时，若 rx_interrupt 先到则发通知）。
/// 中断时保存部分结果（发增量事件），落库，结束本轮。
async fn run_turn(
    ctx: &SessionCtx,
    session: &mut Session,
    rx_interrupt: &mut Receiver<InterruptMessage>,
    content: String,
    params: MessageParams,
) {
    // 1. User 消息进内存历史
    session.messages.push(Message::user(content.clone()));

    // 2. 发 User 事件（回显给 UI）
    ctx.emitter
        .emit(OutputEvent::User(OutputUserMessage {
            base: EventBase::default(),
            payload: fuyao_api::message::output::UserPayload {
                content: content.clone(),
                mode: UserMessageMode::Guide,
                source: fuyao_api::UserMessageSource::User,
            },
        }))
        .await;

    let (model, options) = build_model_and_options(&params, &ctx.tools);

    // ReAct 循环：想 → 可能调工具 → 再想 → ... → 最终回复
    loop {
        let request = build_chat_request(session);
        let mut decoder = StreamDecoder::new();
        let state: SharedTurnState = Arc::new(std::sync::Mutex::new(TurnState::new()));

        // 中断点①：流式期间
        let stream_result = {
            let stream_fut = stream::run_stream_session(
                request,
                &model,
                options.clone(),
                &ctx.provider,
                &ctx.emitter,
                &mut decoder,
                &state,
            );
            tokio::pin!(stream_fut);
            tokio::select! {
                result = &mut stream_fut => result,
                // 中断通道独立：此处只会收到 Interrupt，不会误取 User/Plugin
                interrupt_msg = rx_interrupt.recv() => {
                    if let Some(interrupt_msg) = interrupt_msg {
                        emit_interrupt_event(&interrupt_msg.payload, &ctx.emitter).await;
                        let kind = {
                            let s = state.lock().unwrap_or_else(|e| e.into_inner());
                            classify(&s)
                        };
                        handle_interrupt(&state, kind, &interrupt_msg.payload, &ctx.emitter).await;
                        persist(ctx.emitter.session_id(), session, &ctx.store).await;
                        return;
                    }
                    // 中断通道关闭（所有 tx_interrupt drop）：忽略，继续等流式
                    continue;
                }
            }
        };

        match stream_result {
            Ok(result) => {
                if result.tool_calls.is_empty() {
                    // 无工具调用：最终回复，发 AssistantMessage，落库，结束本轮
                    let assistant_msg =
                        build_assistant_message(&result, params.model_config.model_id.as_deref());
                    session.messages.push(assistant_msg);

                    ctx.emitter
                        .emit(OutputEvent::Assistant(AssistantMessage {
                            base: EventBase::default(),
                            payload: assistant_msg_to_payload(&result),
                        }))
                        .await;

                    persist(ctx.emitter.session_id(), session, &ctx.store).await;
                    return;
                } else {
                    // 有工具调用：发 AssistantMessage(含 tool_calls) → 执行工具 → 结果进 messages → continue
                    let assistant_msg = build_assistant_message_with_tool_calls(
                        &result,
                        params.model_config.model_id.as_deref(),
                    );
                    session.messages.push(assistant_msg);

                    ctx.emitter
                        .emit(OutputEvent::Assistant(AssistantMessage {
                            base: EventBase::default(),
                            payload: assistant_with_tool_calls_to_payload(&result),
                        }))
                        .await;

                    // 中断点②：工具执行期间
                    let tool_results = {
                        let exec_fut = tool_exec::execute_tools(
                            &result.tool_calls,
                            &ctx.tools,
                            &ctx.agent_paths,
                            &ctx.emitter,
                        );
                        tokio::select! {
                            results = exec_fut => results,
                            interrupt_msg = rx_interrupt.recv() => {
                                if let Some(interrupt_msg) = interrupt_msg {
                                    emit_interrupt_event(&interrupt_msg.payload, &ctx.emitter).await;
                                    // 工具执行期间中断：为所有 tool_calls 发中断式 ToolResult
                                    // （execute_tools 是完成一个 emit 一个，已完成的已发；
                                    //  select! drop exec_fut 时未完成的丢失，这里补发中断式）
                                    for tc in &result.tool_calls {
                                        ctx.emitter.emit(make_interrupt_tool_result_event(
                                            tc.id.clone(),
                                            tc.name.clone(),
                                            &interrupt_msg.payload.source,
                                            &interrupt_msg.payload.reason,
                                        )).await;
                                    }
                                    persist(ctx.emitter.session_id(), session, &ctx.store).await;
                                    return;
                                }
                                continue;
                            }
                        }
                    };

                    // 工具结果进 task 本地 messages（不走队列）
                    for tr in &tool_results {
                        session.messages.push(Message::tool_result(
                            tr.tool_call_id.clone(),
                            tr.content.clone(),
                        ));
                    }

                    // continue 回 ReAct 顶部，下一轮带工具结果再问 LLM
                    continue;
                }
            }
            Err(_) => {
                // stream 已发过 Error 事件，这里只落库部分结果
                tracing::warn!(
                    session_id = ctx.emitter.session_id(),
                    "LLM 调用失败，本轮未产出 Assistant 消息"
                );
                persist(ctx.emitter.session_id(), session, &ctx.store).await;
                return;
            }
        }
    }
}

/// 落库（边界时刻调用）
async fn persist(
    session_id: &str,
    session: &mut Session,
    store: &Arc<fuyao_session::SessionStore>,
) {
    session.message_count = session.messages.len() as i64;
    if let Err(e) = store.update(session).await {
        tracing::warn!(session_id = session_id, cause = %e, "session 落库失败");
    }
}

/// 从 session 的内存历史凑 ChatRequest
///
/// 系统提示词单独填 request.system（不进 messages 数组），
/// messages 只装 user/assistant/tool 对话历史。
fn build_chat_request(session: &Session) -> ChatRequest {
    let messages = session
        .messages
        .iter()
        .map(|m| ChatMessage {
            role: m.role.clone(),
            content: m.content.clone(),
            reasoning: m.reasoning.clone(),
            tool_calls: m.tool_calls.as_ref().and_then(|tc| tc.as_array().cloned()),
            tool_call_id: m.tool_call_id.clone(),
            tool_name: m.tool_name.clone(),
        })
        .collect();

    ChatRequest {
        messages,
        system: session.system_prompt.clone(),
    }
}

/// 从 MessageParams 解析模型名 + 构建流式选项
///
/// model_id 格式 "provider/model" → 取 '/' 后的 model 部分。
/// 思考控制参数（thinking_type / reasoning_effort）透传给 StreamOptions。
/// 工具定义从 registry 序列化（非空时带 tools 字段）。
// TODO: model_id 为 None 时用配置的默认模型（当前空串，provider 自行处理）
fn build_model_and_options(
    params: &MessageParams,
    tools: &ToolRegistry,
) -> (String, StreamOptions) {
    let model = params
        .model_config
        .model_id
        .as_deref()
        .and_then(|id| id.split('/').nth(1))
        .unwrap_or("")
        .to_string();

    let tool_defs = tools.definitions_json();
    let options = StreamOptions {
        temperature: None,
        tools: if tool_defs.is_empty() {
            None
        } else {
            Some(tool_defs)
        },
        tool_choice: None,
        thinking_type: params.model_config.thinking_type.clone(),
        reasoning_effort: params.model_config.reasoning_effort.clone(),
    };

    (model, options)
}

/// 从流式结果构建 Assistant Message（无工具调用，最终回复）
fn build_assistant_message(result: &StreamResult, model_id: Option<&str>) -> Message {
    let mut msg = Message::assistant(if result.text.is_empty() {
        None
    } else {
        Some(result.text.clone())
    });
    if !result.reasoning.is_empty() {
        msg.reasoning = Some(result.reasoning.clone());
    }
    msg.model_id = model_id.map(|s| s.to_string());
    msg.finish_reason = Some("stop".to_string());
    msg
}

/// 从流式结果构建含 tool_calls 的 Assistant Message（用于进内存历史）
///
/// tool_calls 转成 OpenAI 格式 JSON：[{id, type:"function", function:{name, arguments}}]
fn build_assistant_message_with_tool_calls(
    result: &StreamResult,
    model_id: Option<&str>,
) -> Message {
    let tool_calls_json: Vec<serde_json::Value> = result
        .tool_calls
        .iter()
        .map(|tc| {
            serde_json::json!({
                "id": tc.id,
                "type": "function",
                "function": {
                    "name": tc.name,
                    "arguments": tc.arguments,
                }
            })
        })
        .collect();

    let mut msg = Message::assistant(if result.text.is_empty() {
        None
    } else {
        Some(result.text.clone())
    });
    if !result.reasoning.is_empty() {
        msg.reasoning = Some(result.reasoning.clone());
    }
    msg.tool_calls = Some(serde_json::Value::Array(tool_calls_json));
    msg.model_id = model_id.map(|s| s.to_string());
    msg.finish_reason = Some("tool_calls".to_string());
    msg
}

/// 从流式结果构建 AssistantPayload（无工具调用，最终回复事件）
fn assistant_msg_to_payload(result: &StreamResult) -> AssistantPayload {
    AssistantPayload {
        content: if result.text.is_empty() {
            None
        } else {
            Some(result.text.clone())
        },
        reasoning: if result.reasoning.is_empty() {
            None
        } else {
            Some(result.reasoning.clone())
        },
        tool_calls: None,
        finish_reason: Some("stop".to_string()),
        completion_tokens: 0,
        prompt_tokens: 0,
        total_tokens: 0,
        reasoning_tokens: 0,
        cached_tokens: 0,
    }
}

/// 从流式结果构建含 tool_calls 的 AssistantPayload（工具调用事件）
fn assistant_with_tool_calls_to_payload(result: &StreamResult) -> AssistantPayload {
    let tool_call_payloads: Vec<ToolCallPayload> = result
        .tool_calls
        .iter()
        .map(|tc| ToolCallPayload {
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            tool_args: serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null),
        })
        .collect();

    AssistantPayload {
        content: if result.text.is_empty() {
            None
        } else {
            Some(result.text.clone())
        },
        reasoning: if result.reasoning.is_empty() {
            None
        } else {
            Some(result.reasoning.clone())
        },
        tool_calls: Some(tool_call_payloads),
        finish_reason: Some("tool_calls".to_string()),
        completion_tokens: 0,
        prompt_tokens: 0,
        total_tokens: 0,
        reasoning_tokens: 0,
        cached_tokens: 0,
    }
}

/// 构建中断式 ToolResult 事件
fn make_interrupt_tool_result_event(
    tool_call_id: String,
    tool_name: String,
    source: &fuyao_api::InterruptSource,
    reason: &str,
) -> OutputEvent {
    OutputEvent::ToolResult(fuyao_api::message::output::ToolResultMessage {
        base: EventBase::default(),
        payload: fuyao_api::message::output::ToolResultPayload {
            tool_call_id,
            tool_name,
            content: format!("[{source:?}][{reason}]"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures_util::stream;
    use fuyao_provider::{
        BoxStream, ChatResponse, FinishReason, StreamError, StreamEvent, StreamUsage,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock Provider：按预设序列依次返回不同的 StreamEvent 列表
    ///
    /// 每次 stream_chat 调用消费 responses 里的一项（支持 ReAct 多轮：第一轮返回工具调用，
    /// 第二轮返回最终回复）。responses 用完则返回空流。
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
            _request: ChatRequest,
            _model: &str,
            _options: StreamOptions,
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
            _request: ChatRequest,
            _model: &str,
        ) -> Result<ChatResponse, StreamError> {
            Err(StreamError::ApiError("mock: chat 不支持".into()))
        }
    }

    /// 构造临时 SessionStore（隔离的临时 DB 目录，每次唯一路径）
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

    /// 构造测试用 SessionCtx + session + rx_interrupt + 收事件的 rx
    #[allow(dead_code)]
    struct TestHarness {
        ctx: SessionCtx,
        session: Session,
        rx_interrupt: Receiver<InterruptMessage>,
        tx_interrupt: tokio::sync::mpsc::Sender<InterruptMessage>,
        rx_event: tokio::sync::mpsc::Receiver<OutputEvent>,
    }

    async fn make_harness(provider: Arc<dyn Provider>, tools: Arc<ToolRegistry>) -> TestHarness {
        let store = temp_store().await;
        let session = Session::new(None, Some("系统提示词".to_string()));
        store.create(&session).await.unwrap();
        let (tx_event, rx_event) = tokio::sync::mpsc::channel(128);
        let (tx_interrupt, rx_interrupt) = tokio::sync::mpsc::channel(8);
        let ctx = SessionCtx {
            store,
            provider,
            tools,
            agent_paths: fuyao_api::AgentPaths::default(),
            emitter: Emitter::new(tx_event, "test_session".to_string()),
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
    async fn collect_events(rx: &mut tokio::sync::mpsc::Receiver<OutputEvent>) -> Vec<OutputEvent> {
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        events
    }

    /// 无工具单轮：User → Chunk → AssistantMessage
    #[tokio::test]
    async fn single_turn_no_tools() {
        let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response("你好")]));
        let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;

        run_turn(
            &h.ctx,
            &mut h.session,
            &mut h.rx_interrupt,
            "用户问题".to_string(),
            MessageParams::default(),
        )
        .await;

        // 验证 session_id 标签全程跟随
        let events = collect_events(&mut h.rx_event).await;
        let has_assistant = events.iter().any(|e| {
            matches!(
                e,
                OutputEvent::Assistant(m) if m.payload.content.as_deref() == Some("你好")
            )
        });
        assert!(has_assistant, "应有 AssistantMessage 事件含「你好」");

        // 验证所有事件带 session_id
        for ev in &events {
            assert_eq!(event_session_id(ev), Some("test_session"));
        }

        // 验证消息历史：user + assistant
        assert_eq!(h.session.messages.len(), 2);
        assert_eq!(h.session.messages[0].role, "user");
        assert_eq!(h.session.messages[1].role, "assistant");
    }

    /// 有工具循环：先返回工具调用 → 执行 echo → 再返回最终回复
    #[tokio::test]
    async fn react_loop_with_tool() {
        let provider = Arc::new(MockProvider::new(vec![
            MockProvider::tool_call_response("call_1", "echo", r#"{"msg":"hi"}"#),
            MockProvider::text_response("工具执行完毕"),
        ]));
        let mut h = make_harness(provider, echo_registry()).await;

        run_turn(
            &h.ctx,
            &mut h.session,
            &mut h.rx_interrupt,
            "调工具".to_string(),
            MessageParams::default(),
        )
        .await;

        let events = collect_events(&mut h.rx_event).await;
        // 应有 ToolResult 事件（echo 工具执行结果）
        let has_tool_result = events.iter().any(|e| {
            matches!(
                e,
                OutputEvent::ToolResult(m) if m.payload.tool_name == "echo"
            )
        });
        assert!(has_tool_result, "应有 ToolResult 事件");

        // 验证消息历史：user + assistant(tool_calls) + tool(结果) + assistant(最终)
        // 注意：流式期间可能因 select! 分支顺序导致 ToolCallChunk 事件丢失，
        //       但消息历史必须完整
        assert!(
            h.session.messages.len() >= 3,
            "消息历史应含 user/assistant/tool 至少 3 条"
        );
        // 最后一条应是 assistant（最终回复）
        let last = h.session.messages.last().unwrap();
        assert_eq!(last.role, "assistant");
    }

    /// 工具结果进 messages（验证 tool_call_id 回填）
    #[tokio::test]
    async fn tool_result_in_messages() {
        let provider = Arc::new(MockProvider::new(vec![
            MockProvider::tool_call_response("tc_42", "echo", r#"{"x":1}"#),
            MockProvider::text_response("完成"),
        ]));
        let mut h = make_harness(provider, echo_registry()).await;

        run_turn(
            &h.ctx,
            &mut h.session,
            &mut h.rx_interrupt,
            "test".to_string(),
            MessageParams::default(),
        )
        .await;

        // 找 role=tool 的消息，验证 tool_call_id
        let tool_msg = h
            .session
            .messages
            .iter()
            .find(|m| m.role == "tool")
            .expect("应有 tool 角色消息");
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("tc_42"));
        assert!(tool_msg.content.as_ref().unwrap().contains("echo"));
    }

    /// LLM 错误：失败直接发 Error 事件，不重试，不产出 AssistantMessage
    #[tokio::test]
    async fn llm_error_emits_error_event() {
        let provider = Arc::new(MockProvider::new(vec![vec![Err(StreamError::AuthError(
            "无效密钥".into(),
        ))]]));
        let mut h = make_harness(provider, Arc::new(ToolRegistry::builder().build())).await;

        run_turn(
            &h.ctx,
            &mut h.session,
            &mut h.rx_interrupt,
            "test".to_string(),
            MessageParams::default(),
        )
        .await;

        let events = collect_events(&mut h.rx_event).await;
        // 应有不可恢复的 Error 事件
        let has_error = events
            .iter()
            .any(|e| matches!(e, OutputEvent::Error(m) if !m.payload.recoverable));
        assert!(has_error, "应有 recoverable=false 的 Error 事件");
        // 不应有 AssistantMessage（失败不产出回复）
        let has_assistant = events
            .iter()
            .any(|e| matches!(e, OutputEvent::Assistant(_)));
        assert!(!has_assistant, "错误时不应产出 AssistantMessage");
    }

    /// 提取事件的 session_id（用于验证全程标签）
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
}
