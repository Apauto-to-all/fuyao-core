//! 引擎核心
//!
//! 两层分离的引擎层：启动一次，装配能力（provider / store / 出口通道）；
//! 多个对话按需创建，各自独立跑交互。
//!
//! 并发模型：每 session 一个独立 tokio task（异步并发）。
//! Engine 持调度表，各 task 并发跑，同 session 内单 task 串行。

pub(crate) mod types;

use crate::engine::types::{SessionHandle, SharedQueue};
use crate::error::EngineError;
use crate::react;
use crate::tool_registry::ToolRegistry;
use fuyao_api::message::input::{InterruptMessage, PluginEventSource, PluginMessage};
use fuyao_api::{EngineParams, InputEvent, MessageParams, OutputEvent, Session, SessionParams};
use fuyao_hooks::{HooksRegistry, PluginHost, SessionSender, SharedHooks};
use fuyao_prompt::build_system_prompt;
use fuyao_session::SessionStore;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, mpsc};
pub use types::SessionId;

/// 引擎
///
/// 能力共享层：构造时装配一次，持有 DB 句柄、provider、出口通道等引擎级共享。
/// 多个对话（Session）共享同一个 Engine 实例，靠 session id 区分。
///
/// 公开 API 遵循设计文档的四个动作：
/// - [`new`](Self::new)：启动引擎（构造即启动）
/// - [`create_session`](Self::create_session)：创建对话
/// - [`resume_session`](Self::resume_session)：恢复对话
/// - [`send`](Self::send)：入事件（单一入口）
/// - [`recv`](Self::recv)：出事件（单一出口）
/// - [`shutdown`](Self::shutdown)：关闭引擎（独立方法，不走消息流）
pub struct Engine {
    /// 会话存储层（Arc 共享给各 session task）
    store: Arc<SessionStore>,

    /// LLM 提供者（Arc 共享给各 session task）
    provider: Arc<dyn fuyao_provider::Provider>,

    /// 工具注册表（引擎级共享，所有 session task 共用同一份）
    tools: Arc<ToolRegistry>,

    /// 插件工厂集合（引擎级共享，每 session 装配时调 create_instances）
    ///
    /// 引擎级只持有工厂模板（无 per-session 状态）；每个 session 启动时
    /// 调用 [`PluginHost::create_instances`] 生成该 session 的独立实例集合，
    /// 各实例的 register 注册到该 session 私有的 HooksRegistry。
    /// 多 session 并发时互不串台（每个 session 都有独立的 hook 状态）。
    plugin_host: Arc<PluginHost>,

    /// 活跃 session 调度表（session_id → SessionHandle）
    sessions: Mutex<std::collections::HashMap<SessionId, SessionHandle>>,

    /// 事件出口通道发送端（单一出口，各 task 往这发）
    tx_event: mpsc::Sender<OutputEvent>,

    /// 事件出口通道接收端（recv 用，Mutex 包裹因为 Engine 可跨 await 持有）
    rx_event: Mutex<mpsc::Receiver<OutputEvent>>,

    /// 引擎启动参数（引擎级，含 agent_paths 等，后续可拓展）
    params: EngineParams,
}

impl Engine {
    /// 启动引擎（动作一）
    ///
    /// 构造即启动：用 `params.agent_paths` 打开数据库、装配 provider 与工具注册表、
    /// 建立出口通道。启动完成后才可创建/恢复对话。
    ///
    /// `tools` 由装配方注入（如从 `fuyao_tools::all_tools()` 转换），引擎持有后
    /// 所有 session task 共享同一份工具表。
    ///
    /// `plugin_host` 是插件工厂集合，引擎级共享。每个 session 启动时调用
    /// [`PluginHost::create_instances`] 生成该 session 的独立实例，
    /// 各实例 register 到该 session 私有的 HooksRegistry。拦截/观察在 session task 内执行。
    pub async fn new(
        params: EngineParams,
        provider: Arc<dyn fuyao_provider::Provider>,
        tools: ToolRegistry,
        plugin_host: PluginHost,
    ) -> Self {
        // 用 agent_paths 解析 db_path，打开数据库
        let db_path = params.agent_paths.sessions_db_path();
        let store = SessionStore::new(db_path)
            .await
            .expect("打开会话数据库失败");

        // 建出口通道（单一出口）
        // TODO: 通道容量从配置读取（第二步先用固定值）
        let (tx_event, rx_event) = mpsc::channel(256);

        Self {
            store: Arc::new(store),
            provider,
            tools: Arc::new(tools),
            plugin_host: Arc::new(plugin_host),
            sessions: Mutex::new(std::collections::HashMap::new()),
            tx_event,
            rx_event: Mutex::new(rx_event),
            params,
        }
    }

    /// 创建对话（动作二）
    ///
    /// 从零创建一个新 Session：构建系统提示词、生成编号、登记进调度表、落库。
    /// `SessionParams` 创建时定死且不可变（Agent 配置改了会冲掉前缀缓存）。
    ///
    /// 返回新 session id。
    pub async fn create_session(&self, params: SessionParams) -> Result<SessionId, EngineError> {
        // 构建系统提示词（Agent 配置决定人格）
        let system_prompt = build_system_prompt(&self.params.agent_paths, &params.agent_config);

        // 创建 Session（8 位 UUID）
        let mut session = Session::new(None, Some(system_prompt));

        // 落库
        self.store.create(&session).await?;

        let session_id = session.id.clone();
        let messages = std::mem::take(&mut session.messages);
        // task 接管 session（含 system_prompt + messages）
        let session = Session {
            messages,
            ..session
        };

        // 装配 session（建队列/通道 + 装配 hooks + spawn task + 登记）
        let handle = self.assemble_session(session_id.clone(), session).await;
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), handle);

        tracing::info!(session_id = %session_id, "创建对话");
        Ok(session_id)
    }

    /// 恢复对话（动作三）
    ///
    /// 把数据库里的老对话捞回内存：用 session id 从 store 加载历史，
    /// 装进内存，重新登记进调度表。
    ///
    /// session id 不在数据库 → 同步返回 `Err(SessionNotFound)`。
    pub async fn resume_session(&self, id: &SessionId) -> Result<(), EngineError> {
        // 从数据库加载
        let session = self
            .store
            .get(id)
            .await?
            .ok_or_else(|| EngineError::SessionNotFound(id.clone()))?;

        // 装配 session（建队列/通道 + 装配 hooks + spawn task + 登记）
        let handle = self.assemble_session(id.clone(), session).await;
        self.sessions.lock().await.insert(id.clone(), handle);

        tracing::info!(session_id = %id, "恢复对话");
        Ok(())
    }

    /// 装配 session（create_session / resume_session 公共方法）
    ///
    /// 建该 session 专属的双队列 + 四条通道（inbound/interrupt/plugin + 事件出口），
    /// 装配该 session 的 hooks（per-session 独立实例），spawn 执行流 task，
    /// 返回 SessionHandle 由调用方登记进调度表。
    ///
    /// 关键：`assemble_session_hooks` 必须 async（`init_send_inputs` 是 async），
    /// 故本方法也是 async。
    async fn assemble_session(&self, session_id: SessionId, session: Session) -> SessionHandle {
        // 双队列
        let guide: SharedQueue = Arc::new(StdMutex::new(std::collections::VecDeque::new()));
        let pending: SharedQueue = Arc::new(StdMutex::new(std::collections::VecDeque::new()));

        // 三条 session 级通道
        let (tx_inbound, rx_inbound) = mpsc::channel::<fuyao_api::InboundUser>(16);
        let (tx_interrupt, rx_interrupt) = mpsc::channel::<InterruptMessage>(8);
        let (tx_plugin, rx_plugin) = mpsc::channel::<PluginMessage>(16);

        // 装配该 session 的 hooks（per-session：create_instances + register + SessionSender）
        let hooks = self
            .assemble_session_hooks(
                &session_id,
                tx_inbound.clone(),
                tx_interrupt.clone(),
                tx_plugin.clone(),
            )
            .await;

        // spawn 执行流 task（多传 rx_plugin 参数）
        let task = tokio::spawn(react::run_session(
            session_id.clone(),
            Arc::clone(&guide),
            Arc::clone(&pending),
            rx_inbound,
            rx_interrupt,
            rx_plugin,
            session,
            Arc::clone(&self.store),
            Arc::clone(&self.provider),
            Arc::clone(&self.tools),
            hooks,
            self.params.agent_paths.clone(),
            self.tx_event.clone(),
        ));

        SessionHandle {
            guide,
            pending,
            tx_inbound,
            tx_interrupt,
            tx_plugin,
            task,
        }
    }

    /// 装配某 session 的 hooks（per-session，每 session 调用一次）
    ///
    /// 流程：
    /// 1. `plugin_host.create_instances()` 生成该 session 的所有插件实例（含重名检查 + create_instance panic 防护）
    /// 2. 每个 `instance.register(&mut registry)` 注册到该 session 私有的 registry（register panic 单独防护）
    /// 3. 构造 `SessionSender`（绑定该 session 的三条通道）
    /// 4. `registry.init_send_inputs(sender).await` 把 sender 传给 send_input hook
    /// 5. 包成 `SharedHooks` 返回
    ///
    /// 失败容错：插件实例化失败（重名等）该 session 以**空 hooks** 运行（不硬 panic，让 session 还能用）。
    async fn assemble_session_hooks(
        &self,
        session_id: &SessionId,
        tx_inbound: mpsc::Sender<fuyao_api::InboundUser>,
        tx_interrupt: mpsc::Sender<InterruptMessage>,
        tx_plugin: mpsc::Sender<PluginMessage>,
    ) -> SharedHooks {
        let mut registry = HooksRegistry::new();

        // 1. 工厂生产实例（同步，host 内部已含 create_instance panic 防护）
        let instances = match self.plugin_host.create_instances() {
            Ok(insts) => insts,
            Err(e) => {
                tracing::error!(
                    session_id = %session_id,
                    cause = %e,
                    "插件实例化失败（重名或装配错误），该 session 将以空 hooks 运行"
                );
                return Arc::new(Mutex::new(registry));
            }
        };

        // 2. 每个 instance 注册 hook（register 是同步调用，单独 panic 防护）
        //    单个 instance.register panic 不阻塞其他实例注册
        for (idx, instance) in instances.iter().enumerate() {
            let hint = format!("instance-{idx}");
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                instance.register(&mut registry)
            }));
            if let Err(payload) = result {
                tracing::warn!(
                    session_id = %session_id,
                    hint = %hint,
                    phase = "register",
                    recovered = true,
                    cause = %fuyao_hooks::panic_payload_to_string(&*payload),
                    "插件实例 register panic 已恢复"
                );
            }
        }

        // 3. 构造 SessionSender（identity 用 session_id 占位）
        //    注：各插件发 Plugin 消息的精确身份由插件通过 send_plugin_full 等方法控制，
        //    或后续给 SessionSender 加 with_identity 方法优化
        let sender = SessionSender::new(
            PluginEventSource {
                name: format!("session:{session_id}"),
            },
            tx_inbound,
            tx_interrupt,
            tx_plugin,
        );

        // 4. 把 sender 传给所有 send_input hook
        registry.init_send_inputs(sender).await;

        // 5. 包成 SharedHooks
        Arc::new(Mutex::new(registry))
    }

    /// 入事件（单一入口）
    ///
    /// 所有对话级输入事件从一个口进，靠 session id 区分对话，按事件类型分流：
    /// - `User`：入队，触发 ReAct 循环。`MessageParams` 决定本轮用哪个模型
    /// - `Interrupt`：发出中断信号，打断对应对话的当前执行
    /// - `Plugin`：插件发给某对话的通知，转发为 OutputEvent::Plugin 送出
    ///
    /// 入队即返回，不阻塞——不等大模型想完。
    /// 后续产出从 [`recv`](Self::recv) 流出。
    ///
    /// session id 不在调度表 → 同步返回 `Err(SessionNotFound)`（要恢复走恢复动作）。
    ///
    /// `MessageParams` 决定本轮用哪个模型、怎么思考——model id 跟着消息走。
    /// 仅 `User` 变体使用，其他变体忽略此参数。
    pub async fn send(
        &self,
        id: &SessionId,
        event: InputEvent,
        params: MessageParams,
    ) -> Result<(), EngineError> {
        let sessions = self.sessions.lock().await;
        let handle = sessions
            .get(id)
            .ok_or_else(|| EngineError::SessionNotFound(id.clone()))?;

        match event {
            InputEvent::User(user_msg) => {
                // User 消息经入站通道送进 session task，由管道处理：
                // 拦截 → 处理(入 guide/pending 队列) → 发送(回显 User 给 UI) → 观察。
                // 不在引擎层直接操作队列——入队是 session 层管道的 process 职责。
                let inbound = fuyao_api::InboundUser {
                    content: user_msg.payload.content,
                    mode: user_msg.payload.mode,
                    params,
                };
                handle
                    .tx_inbound
                    .send(inbound)
                    .await
                    .map_err(|_| EngineError::Shutdown)?;
            }
            InputEvent::Interrupt(interrupt_msg) => {
                // 中断走独立通道（select! 中断点监听）
                handle
                    .tx_interrupt
                    .send(interrupt_msg)
                    .await
                    .map_err(|_| EngineError::Shutdown)?;
            }
            InputEvent::Plugin(plugin_msg) => {
                // 插件通知送进 session 的 Plugin 通道，由 session task 过 dispatch 管道：
                // 拦截 → 发送（盖 session_id 标签发外部） → 观察
                // 不在 Engine 层直接发 OutputEvent::Plugin——所有消息统一经 session task 的管道
                handle
                    .tx_plugin
                    .send(plugin_msg)
                    .await
                    .map_err(|_| EngineError::Shutdown)?;
            }
        }

        Ok(())
    }

    /// 关闭引擎
    ///
    /// 引擎关闭是危险操作，不混入对话级的事件流（不走 send），
    /// 由独立的关闭方法触发。
    ///
    /// 关闭流程（后续实现）：
    /// - 停止接收新的对话级事件
    /// - 等待所有活跃 session 的当前执行完成或优雅中断
    /// - 落库未持久化的状态
    /// - 关闭 DB 连接、释放资源
    // TODO: 实现引擎关闭流程（drop 所有 session task + 落库 + 释放资源）
    pub async fn shutdown(&self) {
        // 第二步：清空调度表，drop 所有 SessionHandle（task 句柄 drop 不 abort，但通道关闭后 task 自然退出）
        let mut sessions = self.sessions.lock().await;
        sessions.clear();
        tracing::info!("引擎关闭（session 调度表已清空）");
    }

    /// 出事件（单一出口）
    ///
    /// 从统一出口取下一条产出事件，按 session_id 归类到对应对话。
    /// 所有对话的产出都从此口流出，没有第二个出口。
    ///
    /// 返回 `None` 表示引擎已关闭、通道已断。
    pub async fn recv(&self) -> Option<OutputEvent> {
        self.rx_event.lock().await.recv().await
    }
}
