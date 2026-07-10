//! Engine（InputDispatcher 角色）
//!
//! 引擎本体，后台任务运行，持续监听输入消息并分发：
//! - User → 拦截（dispatch_intercept）+ 入队 + notify 唤醒；消费时才 deliver
//! - Interrupt → dispatch（一气呵成）+ 发送 Interrupt 命令
//! - Plugin → dispatch（一气呵成）
//! - Shutdown → 发送 Stop 命令 + 退出

use crate::engine::emitter::EventEmitter;
use crate::engine::turn_executor::TurnExecutor;
use crate::engine::types::{
    SharedGuideQueue, SharedHooks, SharedPendingQueue, SharedTools, TurnCommand,
};
use crate::handle::{EngineHandle, HandleParams, new_handle};
use fuyao_api::message::output;
use fuyao_api::message::{InputEvent, OutputEvent, QueueUpdateKind, UserMessageMode};
use fuyao_api::{AgentContext, SharedAgentCtx, ToolFn};
use fuyao_provider::Provider as LlmProvider;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct Engine {
    /// 输入事件接收端
    pub(crate) rx_input: mpsc::Receiver<InputEvent>,
    /// 输入事件发送端（用于钩子发送 InputEvent）
    pub(crate) tx_input: mpsc::Sender<InputEvent>,
    /// 命令通道发送端
    pub(crate) tx_command: mpsc::Sender<TurnCommand>,
    /// 统一事件发送器
    pub(crate) emitter: EventEmitter,
    /// 共享工具注册表
    pub(crate) tools: SharedTools,
    /// 引导队列
    pub(crate) guide_queue: SharedGuideQueue,
    /// 排队队列
    pub(crate) pending_queue: SharedPendingQueue,
    /// 队列更新通知器
    pub(crate) queue_notify: Arc<tokio::sync::Notify>,
    /// TurnExecutor 任务句柄（用于监督和优雅退出）
    pub(crate) turn_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Engine {
    /// 创建引擎，返回 (Engine, EngineHandle)
    ///
    /// 内部 spawn TurnExecutor 常驻任务，Engine 自身作为 InputDispatcher。
    /// 通道容量从全局配置 `get_config().engine` 读取。
    pub fn new(provider: Box<dyn LlmProvider>, agent_ctx: AgentContext) -> (Self, EngineHandle) {
        let engine_cfg = fuyao_api::get_config().engine.clone();
        let (tx_input, rx_input) = mpsc::channel(engine_cfg.input_channel_capacity);
        let (tx_event, rx_event) = mpsc::channel(engine_cfg.output_channel_capacity);
        let (tx_command, rx_command) = mpsc::channel(engine_cfg.command_channel_capacity);

        let shared_agent_ctx: SharedAgentCtx = Arc::new(std::sync::Mutex::new(agent_ctx));
        let shared_tools: SharedTools =
            Arc::new(std::sync::Mutex::new((HashMap::new(), Vec::new())));
        let shared_hooks: SharedHooks =
            Arc::new(tokio::sync::Mutex::new(fuyao_hooks::HooksRegistry::new()));

        // 双队列 + 通知器
        let guide_queue: SharedGuideQueue =
            Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let pending_queue: SharedPendingQueue =
            Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let queue_notify = Arc::new(tokio::sync::Notify::new());

        let emitter = EventEmitter::new(tx_event.clone(), shared_hooks.clone());

        // 创建并 spawn TurnExecutor（常驻任务）
        let turn_executor = TurnExecutor {
            provider,
            agent_ctx: shared_agent_ctx.clone(),
            tools: shared_tools.clone(),
            rx_command: Some(rx_command),
            emitter: emitter.clone(),
            guide_queue: guide_queue.clone(),
            pending_queue: pending_queue.clone(),
            queue_notify: queue_notify.clone(),
        };
        let turn_handle = tokio::spawn(async move {
            let mut executor = turn_executor;
            executor.run().await;
        });

        let engine = Self {
            rx_input,
            tx_input: tx_input.clone(),
            tx_command,
            emitter,
            tools: shared_tools.clone(),
            guide_queue: guide_queue.clone(),
            pending_queue: pending_queue.clone(),
            queue_notify: queue_notify.clone(),
            turn_handle: Some(turn_handle),
        };

        let handle = new_handle(HandleParams {
            tx_input,
            rx_event,
            agent_ctx: shared_agent_ctx,
            tools: shared_tools,
            hooks: shared_hooks,
            tx_event,
            guide_queue,
            pending_queue,
        });

        (engine, handle)
    }

    /// 注册工具
    pub fn register_tool(&self, name: String, schema: serde_json::Value, handler: ToolFn) {
        let mut tools = self.tools.lock().expect("工具注册表锁异常");
        tools.0.insert(name, handler);
        tools.1.push(schema);
    }

    /// InputDispatcher 主循环：持续监听输入消息并分发
    pub async fn run(&mut self) {
        // 初始化 send_input 钩子，传入 Sender
        {
            let mut hooks = self.emitter.hooks().lock().await;
            hooks.init_send_inputs(self.tx_input.clone()).await;
        }

        loop {
            let event = match self.rx_input.recv().await {
                Some(event) => event,
                None => break,
            };

            match event {
                InputEvent::User(user_data) => {
                    // ① 拦截（不 deliver），拿到拦截后的输出 UserMessage
                    let intercepted = crate::dispatch::dispatch_intercept(
                        OutputEvent::User(output::UserMessage {
                            base: user_data.base.clone(),
                            payload: output::UserPayload {
                                content: user_data.payload.content.clone(),
                                mode: user_data.payload.mode,
                                source: user_data.payload.source.clone(),
                            },
                        }),
                        None,
                        &self.emitter,
                    )
                    .await;

                    // 拦截器 Block 时丢弃该消息（不入队）
                    let message = match intercepted {
                        Some(OutputEvent::User(m)) => m,
                        _ => continue,
                    };

                    // ② 打包为 QueuedUserMessage 并按模式入队
                    // 先记录 base.id，入队后用于 QueueUpdate 事件（UI 配对临时气泡）
                    let msg_base = message.base.clone();
                    let queued = crate::engine::types::QueuedUserMessage { user_data, message };
                    match queued.user_data.payload.mode {
                        UserMessageMode::Guide => {
                            self.guide_queue
                                .lock()
                                .expect("引导队列锁异常")
                                .push_back(queued);
                        }
                        UserMessageMode::Pending => {
                            self.pending_queue
                                .lock()
                                .expect("排队队列锁异常")
                                .push_back(queued);
                        }
                    }

                    // ③ 唤醒 TurnExecutor（消费时才 deliver）
                    self.queue_notify.notify_one();

                    // ④ 通知 UI 队列长度变化（入队事件，复用消息 base.id 便于 UI 配对）
                    let guide_count = self.guide_queue.lock().expect("引导队列锁异常").len();
                    let pending_count = self.pending_queue.lock().expect("排队队列锁异常").len();
                    let _ = self
                        .emitter
                        .send(OutputEvent::QueueUpdate(output::QueueUpdateMessage {
                            base: msg_base,
                            payload: output::QueueUpdatePayload {
                                guide_count,
                                pending_count,
                                kind: QueueUpdateKind::Enqueued,
                            },
                        }))
                        .await;
                }
                InputEvent::Interrupt(data) => {
                    // 统一管道：转化 → 拦截 → 处理回调 → 发送 → 观察
                    let tx = self.tx_command.clone();
                    crate::dispatch::dispatch(
                        OutputEvent::Interrupt(output::InterruptMessage {
                            base: data.base.clone(),
                            payload: output::InterruptPayload {
                                reason: data.payload.reason.clone(),
                                source: data.payload.source.clone(),
                            },
                        }),
                        Some(Box::new(move || {
                            if let Err(e) = tx.try_send(TurnCommand::Interrupt(data)) {
                                tracing::warn!(cause = ?e, "中断命令投递失败");
                            }
                        })),
                        &self.emitter,
                    )
                    .await;
                }
                InputEvent::Plugin(data) => {
                    // 统一管道：转化 → 拦截 → 无处理回调 → 发送 → 观察
                    crate::dispatch::dispatch(
                        OutputEvent::Plugin(output::PluginMessage {
                            base: data.base,
                            payload: output::PluginPayload {
                                source: data.payload.source,
                                event_type: data.payload.event_type,
                                data: data.payload.data,
                                error: data.payload.error,
                                message: data.payload.message,
                            },
                        }),
                        None,
                        &self.emitter,
                    )
                    .await;
                }
                InputEvent::Shutdown(_) => {
                    if let Err(e) = self.tx_command.send(TurnCommand::Stop).await {
                        tracing::warn!(cause = %e, "停止命令投递失败");
                    }
                    break;
                }
            }
        }

        // 等待 TurnExecutor 退出
        if let Some(handle) = self.turn_handle.take()
            && let Err(join_err) = handle.await
        {
            tracing::error!(cause = %join_err, is_panic = join_err.is_panic(), "TurnExecutor 任务异常退出");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::message::EventBase;
    use fuyao_api::message::input;
    use fuyao_provider::{
        BoxStream, ChatRequest, ChatResponse, FinishReason, Provider as LlmProvider, StreamError,
        StreamEvent, StreamOptions, StreamUsage,
    };

    /// 测试用 Mock Provider
    struct MockProvider;

    #[async_trait::async_trait]
    impl LlmProvider for MockProvider {
        fn stream_chat(
            &self,
            _request: ChatRequest,
            _model: &str,
            _options: StreamOptions,
        ) -> BoxStream<Result<StreamEvent, StreamError>> {
            let stream = async_stream::stream! {
                yield Ok(StreamEvent::TextDelta { content: "Hello".to_string() });
                yield Ok(StreamEvent::Done {
                    usage: StreamUsage::default(),
                    finish_reason: FinishReason::Stop,
                });
            };
            Box::pin(stream)
        }

        async fn chat(
            &self,
            _request: ChatRequest,
            _model: &str,
        ) -> Result<ChatResponse, StreamError> {
            Ok(ChatResponse {
                content: Some("Hello".to_string()),
                reasoning: None,
                tool_calls: None,
                usage: StreamUsage::default(),
                finish_reason: FinishReason::Stop,
            })
        }
    }

    fn test_agent_ctx() -> AgentContext {
        let mut ctx = AgentContext::default();
        ctx.model_config.model_id = Some("test-model".to_string());
        ctx
    }

    #[tokio::test]
    async fn engine_new_creates_handle() {
        let provider = Box::new(MockProvider);
        let (engine, handle) = Engine::new(provider, test_agent_ctx());
        assert!(!handle.tx_input.is_closed());
        drop(engine);
    }

    #[tokio::test]
    async fn engine_register_tool() {
        let provider = Box::new(MockProvider);
        let (engine, handle) = Engine::new(provider, test_agent_ctx());

        engine.register_tool(
            "test_tool".to_string(),
            serde_json::json!({"type": "function", "function": {"name": "test_tool"}}),
            Arc::new(|_args, _ctx: fuyao_api::ToolCallContext| {
                Box::pin(async { "ok".to_string() })
            }),
        );

        // 通过 EngineHandle 的公有接口验证工具注册结果
        let tools = handle.tools_shared();
        let guard = tools.lock().expect("工具注册表锁异常");
        assert_eq!(guard.0.len(), 1);
        assert_eq!(guard.1.len(), 1);
    }

    /// 验证：User 输出事件复用输入事件的 base.id 与时间戳
    /// 确保 UI 气泡与用户输入能一一对应
    #[tokio::test]
    async fn user_message_output_reuses_input_base() {
        let provider = Box::new(MockProvider);
        let (mut engine, handle) = Engine::new(provider, test_agent_ctx());

        let engine_task = tokio::spawn(async move { engine.run().await });

        // 手工构造带特定 id 与时间戳的输入事件
        let input_id = "11111111-2222-3333-4444-555555555555".to_string();
        let input_ts = 1234567890.5_f64;
        let _ = handle
            .tx_input
            .send(InputEvent::User(input::UserMessage {
                base: EventBase {
                    id: input_id.clone(),
                    timestamp: input_ts,
                },
                payload: input::UserPayload {
                    content: "测试复用".to_string(),
                    mode: fuyao_api::message::UserMessageMode::Guide,
                    source: fuyao_api::message::UserMessageSource::User,
                },
            }))
            .await;

        // 收集事件直到拿到 User
        let user_message = loop {
            match tokio::time::timeout(std::time::Duration::from_secs(2), handle.next_event()).await
            {
                Ok(Some(OutputEvent::User(d))) => break d,
                Ok(Some(_)) => continue,
                _ => panic!("未在超时内收到 User 事件"),
            }
        };

        // 验证：输出事件 base 与输入事件 base 完全一致
        assert_eq!(user_message.base.id, input_id);
        assert_eq!(user_message.base.timestamp, input_ts);
        assert_eq!(user_message.payload.content, "测试复用");

        handle.shutdown().await;
        let _ = engine_task.await;
    }

    #[tokio::test]
    async fn engine_send_message_produces_events() {
        let provider = Box::new(MockProvider);
        let (mut engine, handle) = Engine::new(provider, test_agent_ctx());

        let engine_task = tokio::spawn(async move {
            engine.run().await;
        });

        handle.send_message("Hi".to_string()).await;

        // 收集事件（带超时），事件流：QueueUpdate(Enqueued) → UserMessage →
        // QueueUpdate(Consumed) → TurnStart → Chunk → Assistant → QueueUpdate(后续转移)
        // 至少需要 6 个槽位才能覆盖 Chunk + Assistant
        let mut events = Vec::new();
        for _ in 0..10 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), handle.next_event()).await
            {
                Ok(Some(e)) => events.push(e),
                _ => break,
            }
        }

        handle.shutdown().await;
        let _ = engine_task.await;

        assert!(events.iter().any(|e| matches!(e, OutputEvent::Chunk(_))));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, OutputEvent::Assistant(_)))
        );
    }

    #[tokio::test]
    async fn engine_shutdown_stops_loop() {
        let provider = Box::new(MockProvider);
        let (mut engine, handle) = Engine::new(provider, test_agent_ctx());

        let engine_task = tokio::spawn(async move {
            engine.run().await;
        });

        handle.shutdown().await;
        let result = engine_task.await;
        assert!(result.is_ok());
    }

    /// 验证：用户消息入队后会发出 QueueUpdate(Enqueued) 事件
    /// 携带 base.id 与队列长度快照
    #[tokio::test]
    async fn user_message_emits_queue_update_on_enqueue() {
        let provider = Box::new(MockProvider);
        let (mut engine, handle) = Engine::new(provider, test_agent_ctx());

        let engine_task = tokio::spawn(async move { engine.run().await });

        let input_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string();
        let _ = handle
            .tx_input
            .send(InputEvent::User(input::UserMessage {
                base: EventBase {
                    id: input_id.clone(),
                    timestamp: 0.0,
                },
                payload: input::UserPayload {
                    content: "队列事件测试".to_string(),
                    mode: fuyao_api::message::UserMessageMode::Guide,
                    source: fuyao_api::message::UserMessageSource::User,
                },
            }))
            .await;

        // 收集事件直到拿到首个 QueueUpdate
        let queue_update = loop {
            match tokio::time::timeout(std::time::Duration::from_secs(2), handle.next_event()).await
            {
                Ok(Some(OutputEvent::QueueUpdate(d))) => break d,
                Ok(Some(_)) => continue,
                _ => panic!("未在超时内收到 QueueUpdate 事件"),
            }
        };

        assert_eq!(queue_update.payload.kind, QueueUpdateKind::Enqueued);
        assert_eq!(queue_update.base.id, input_id);
        // 至少有刚入队的那一条
        assert!(queue_update.payload.guide_count >= 1);

        handle.shutdown().await;
        let _ = engine_task.await;
    }
}
