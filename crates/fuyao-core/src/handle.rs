//! 引擎句柄，UI 层持有

use crate::engine::types::{SharedGuideQueue, SharedPendingQueue};
use crate::engine::{SharedHooks, SharedTools};
use fuyao_api::message::input::{
    InterruptMessage, InterruptPayload, InterruptSource, ShutdownMessage, UserMessage, UserPayload,
};
use fuyao_api::message::{EventBase, InputEvent, OutputEvent, UserMessageSource};
use fuyao_api::{AgentContext, QueueSnapshot, QueueSnapshotItem, SharedAgentCtx, ToolFn};
use tokio::sync::mpsc;

/// 引擎句柄，UI 层持有
pub struct EngineHandle {
    /// 发送输入事件
    pub tx_input: mpsc::Sender<InputEvent>,
    /// 接收输出事件
    rx_event: tokio::sync::Mutex<mpsc::Receiver<OutputEvent>>,
    /// 共享 Agent 上下文
    agent_ctx: SharedAgentCtx,
    /// 共享工具注册表
    tools: SharedTools,
    /// 共享钩子注册表
    hooks: SharedHooks,
    /// 事件发送端（用于 emit_plugin_event）
    tx_event: mpsc::Sender<OutputEvent>,
    /// 引导队列共享引用（用于只读快照）
    guide_queue: SharedGuideQueue,
    /// 排队队列共享引用（用于只读快照）
    pending_queue: SharedPendingQueue,
}

impl EngineHandle {
    /// 发送用户消息
    pub async fn send_message(&self, content: String) {
        let result = self
            .tx_input
            .send(InputEvent::User(UserMessage {
                base: EventBase::default(),
                payload: UserPayload {
                    content,
                    mode: Default::default(),
                    source: UserMessageSource::User,
                },
            }))
            .await;
        if result.is_err() {
            tracing::warn!("用户消息发送失败，引擎输入通道已关闭");
        }
    }

    /// 中断当前轮次（通过输入通道发送 Interrupt 事件）
    ///
    /// 唯一入口原则：所有中断都走 InputEvent::Interrupt，
    /// InputDispatcher 收到后转发给 TurnExecutor。
    pub fn cancel(&self) {
        let result = self
            .tx_input
            .try_send(InputEvent::Interrupt(InterruptMessage {
                base: EventBase::default(),
                payload: InterruptPayload {
                    reason: "用户取消".to_string(),
                    source: InterruptSource::User,
                },
            }));
        if result.is_err() {
            tracing::warn!("中断请求发送失败");
        }
    }

    /// 关闭引擎
    pub async fn shutdown(&self) {
        let result = self
            .tx_input
            .send(InputEvent::Shutdown(ShutdownMessage {
                base: EventBase::default(),
            }))
            .await;
        if result.is_err() {
            tracing::warn!("关闭请求发送失败");
        }
    }

    /// 接收下一个事件
    pub async fn next_event(&self) -> Option<OutputEvent> {
        let mut rx = self.rx_event.lock().await;
        rx.recv().await
    }

    /// 获取共享钩子注册表（用于插件加载）
    pub fn hooks(&self) -> SharedHooks {
        self.hooks.clone()
    }

    /// 获取 Agent 上下文（克隆）
    ///
    /// 返回当前 AgentContext 的克隆，调用方可读取任意字段（model_config / agent_paths 等）。
    /// 若需修改，配合 [`set_agent_ctx`](Self::set_agent_ctx) 整体写回。
    pub fn agent_ctx(&self) -> Option<AgentContext> {
        self.agent_ctx.lock().ok().map(|ctx| ctx.clone())
    }

    /// 设置 Agent 上下文（整体替换）
    ///
    /// 典型用法（读-改-写模式，支持运行时切换模型 / 思考参数等）：
    /// ```ignore
    /// let mut ctx = handle.agent_ctx()?;
    /// ctx.model_config.thinking_type = Some(ThinkingType::Enabled);
    /// ctx.model_config.reasoning_effort = Some("high".to_string());
    /// handle.set_agent_ctx(ctx);
    /// ```
    pub fn set_agent_ctx(&self, ctx: AgentContext) {
        match self.agent_ctx.lock() {
            Ok(mut c) => {
                let from = c.model_config.model_id.clone();
                let to = ctx.model_config.model_id.clone();
                if from != to {
                    tracing::info!(from = ?from, to = ?to, "运行时 model 切换");
                }
                *c = ctx;
            }
            Err(_) => tracing::error!("Agent 上下文锁中毒，set_agent_ctx 未生效"),
        }
    }

    /// 注册工具
    pub fn register_tool(&self, name: &str, schema: serde_json::Value, handler: ToolFn) {
        match self.tools.lock() {
            Ok(mut tools) => {
                tools.0.insert(name.to_string(), handler);
                tools.1.push(schema);
            }
            Err(_) => tracing::error!(tool = %name, "工具注册表锁中毒，工具注册未生效"),
        }
    }

    /// 获取工具 schema 列表
    pub fn tools_schema(&self) -> Vec<serde_json::Value> {
        self.tools.lock().map(|t| t.1.clone()).unwrap_or_default()
    }

    /// 获取共享工具注册表（用于插件加载）
    pub fn tools_shared(&self) -> SharedTools {
        self.tools.clone()
    }

    /// 获取共享 Agent 上下文（用于插件加载）
    pub fn agent_ctx_shared(&self) -> SharedAgentCtx {
        self.agent_ctx.clone()
    }

    /// 获取事件发送端克隆（用于适配器）
    pub fn tx_event_shared(&self) -> mpsc::Sender<OutputEvent> {
        self.tx_event.clone()
    }

    /// 发送插件事件
    pub fn emit_plugin_event(&self, event: OutputEvent) {
        if self.tx_event.try_send(event).is_err() {
            tracing::warn!("插件事件发送失败");
        }
    }

    /// 非阻塞尝试接收事件
    pub fn try_next_event(&self) -> Option<OutputEvent> {
        let mut rx = self.rx_event.try_lock().ok()?;
        rx.try_recv().ok()
    }

    /// 获取队列只读快照（用于 UI 显示队列内容，如 /queue 命令）
    ///
    /// 返回当前 guide / pending 队列的浅快照：
    /// - `id`：消息 base.id
    /// - `content_preview`：内容前 30 字符（按 Unicode 字符截断，避免 panic）
    /// - `mode`：消息模式
    pub fn queue_snapshot(&self) -> QueueSnapshot {
        /// 抽取一条消息为只读快照条目
        fn snapshot_content(s: &str) -> String {
            // Unicode 安全截断到 30 字符
            s.chars().take(30).collect()
        }

        let guide = {
            let q = self.guide_queue.lock().expect("引导队列锁异常");
            q.iter()
                .map(|q| QueueSnapshotItem {
                    id: q.message.base.id.clone(),
                    content_preview: snapshot_content(&q.message.payload.content),
                    mode: q.user_data.payload.mode,
                })
                .collect::<Vec<_>>()
        };
        let pending = {
            let q = self.pending_queue.lock().expect("排队队列锁异常");
            q.iter()
                .map(|q| QueueSnapshotItem {
                    id: q.message.base.id.clone(),
                    content_preview: snapshot_content(&q.message.payload.content),
                    mode: q.user_data.payload.mode,
                })
                .collect::<Vec<_>>()
        };
        QueueSnapshot { guide, pending }
    }
}

/// EngineHandle 构造器参数（内部使用）
pub(crate) struct HandleParams {
    pub tx_input: mpsc::Sender<InputEvent>,
    pub rx_event: mpsc::Receiver<OutputEvent>,
    pub agent_ctx: SharedAgentCtx,
    pub tools: SharedTools,
    pub hooks: SharedHooks,
    pub tx_event: mpsc::Sender<OutputEvent>,
    pub guide_queue: SharedGuideQueue,
    pub pending_queue: SharedPendingQueue,
}

/// EngineHandle 构造器（仅 engine_impl 使用）
pub(crate) fn new_handle(params: HandleParams) -> EngineHandle {
    EngineHandle {
        tx_input: params.tx_input,
        rx_event: tokio::sync::Mutex::new(params.rx_event),
        agent_ctx: params.agent_ctx,
        tools: params.tools,
        hooks: params.hooks,
        tx_event: params.tx_event,
        guide_queue: params.guide_queue,
        pending_queue: params.pending_queue,
    }
}
