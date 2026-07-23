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
use fuyao_provider::ProviderRegistry;
use fuyao_session::SessionStore;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;
pub use types::SessionId;

/// shutdown 等待所有 session task 退出的总超时阈值
///
/// task 收到 shutdown_token.cancel() 后，各 task 的 select! shutdown 分支立即胜出，
/// break 前会走中断路径落库（保护 in-flight 状态）——通常毫秒级完成。
/// 所有 task **并发退出**（用 JoinSet 同时 await），共享 10 秒总预算：
/// 到点仍未退出的 task 统一 abort（JoinSet drop 自动 abort 所有未完成 task）。
/// 这是设计文档「显式关闭 + 等待退出 + 强制中止兜底」三层保障中的总超时兜底。
const SHUTDOWN_TASK_TIMEOUT: Duration = Duration::from_secs(10);

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

    /// Provider 实例注册表（多 Provider 路由）
    ///
    /// 引擎级共享：持有所有已注册 Provider 的构造好的实例（按 provider_id 索引）。
    /// 每条消息的 `MessageParams.model_id` 形如 `"provider_id/model_id"`——
    /// session task 在执行该轮 LLM 调用前，从 model_id 拆出 provider_id，
    /// 从本注册表取对应 Provider 实例，实现"不同 session / 不同消息用不同 Provider"。
    ///
    /// 与旧"引擎持单个 `Arc<dyn Provider>`"模型的差异：旧模型启动时按 default
    /// model_id 选一个 Provider，所有调用都打到这里；新模型按消息的 provider_id 路由。
    providers: Arc<ProviderRegistry>,

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

    /// 引擎是否已 shutdown（AtomicBool 同步快路径）
    ///
    /// shutdown 后置 true，作为 send / recv 的同步快路径检查：
    /// - `send` 立即返回 `Err(EngineError::Shutdown)`（区分于 `SessionNotFound`）
    /// - `recv` 先 drain 残余事件，再返回 None（不丢 shutdown 前最后几条事件）
    ///
    /// 用 AtomicBool 而非 CancellationToken：send/recv 入口检查需同步、不 await，
    /// AtomicBool 满足零开销同步语义；task 内的取消信号另用 `shutdown_token`。
    shutdown: Arc<AtomicBool>,

    /// 引擎级关闭信号（cancel 后所有 session task 的 select! 同时收到）
    ///
    /// 每个 session 在 `assemble_session` 时用 `child_token()` 派生子 token，
    /// 既支持引擎级一次 cancel 全部（Engine::shutdown 调 root.cancel），
    /// 也为未来「单 session 销毁」扩展点（cancel 单个 child）预留。
    shutdown_token: CancellationToken,
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
    /// `providers` 是 Provider 实例注册表（多 Provider 路由）：启动时由装配方从
    /// 已注册 Provider 配置批量构造实例（`ProviderRegistry::from_registered`），
    /// 每条消息的 `MessageParams.model_id` 决定本轮走哪个 Provider。
    ///
    /// `plugin_host` 是插件工厂集合，引擎级共享。每个 session 启动时调用
    /// [`PluginHost::create_instances`] 生成该 session 的独立实例，
    /// 各实例 register 到该 session 私有的 HooksRegistry。拦截/观察在 session task 内执行。
    pub async fn new(
        params: EngineParams,
        providers: ProviderRegistry,
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
            providers: Arc::new(providers),
            tools: Arc::new(tools),
            plugin_host: Arc::new(plugin_host),
            sessions: Mutex::new(std::collections::HashMap::new()),
            tx_event,
            rx_event: Mutex::new(rx_event),
            params,
            shutdown: Arc::new(AtomicBool::new(false)),
            shutdown_token: CancellationToken::new(),
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
        let session = Session::new(None, Some(system_prompt));

        // 落库元数据（消息产生时由 emit_to_history 单条 insert_message 落库）
        self.store.create(&session).await?;

        let session_id = session.id.clone();

        // 装配 session（建队列/通道 + 装配 hooks + spawn task + 登记）
        // SessionParams 整体传下去，不在入口拆包——压缩重建 prompt 等运行时场景
        // 仍需 agent_config，贯穿到 SessionCtx 留存，将来加字段只动 SessionCtx 一处
        let handle = self
            .assemble_session(session_id.clone(), session, params)
            .await;
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
    /// 与 `create_session` 对称，同样由调用方提供 `SessionParams`
    /// （含 Agent 配置）——引擎核心不持久化 AgentConfig，恢复时由
    /// 调用方把创建时的那份配置原样再传一次。
    ///
    /// session id 不在数据库 → 同步返回 `Err(SessionNotFound)`。
    pub async fn resume_session(
        &self,
        id: &SessionId,
        params: SessionParams,
    ) -> Result<(), EngineError> {
        // 从数据库加载
        let session = self
            .store
            .get(id)
            .await?
            .ok_or_else(|| EngineError::SessionNotFound(id.clone()))?;

        // 装配 session（建队列/通道 + 装配 hooks + spawn task + 登记）
        // SessionParams 整体传下去（与 create_session 对称）
        let handle = self.assemble_session(id.clone(), session, params).await;
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
    async fn assemble_session(
        &self,
        session_id: SessionId,
        session: Session,
        session_params: SessionParams,
    ) -> SessionHandle {
        // 双队列
        let guide: SharedQueue = Arc::new(StdMutex::new(std::collections::VecDeque::new()));
        let pending: SharedQueue = Arc::new(StdMutex::new(std::collections::VecDeque::new()));

        // 三条 session 级通道
        let (tx_inbound, rx_inbound) = mpsc::channel::<fuyao_api::InboundUser>(16);
        let (tx_interrupt, rx_interrupt) = mpsc::channel::<InterruptMessage>(8);
        let (tx_plugin, rx_plugin) = mpsc::channel::<PluginMessage>(16);

        // 该 session 的关闭信号（引擎级 shutdown_token 的 child_token）
        //   Engine::shutdown 调 root.cancel → 所有 child 同时 cancel
        //   未来扩展「单 session 销毁」时可单独 cancel 这个 child
        let shutdown_token = self.shutdown_token.child_token();

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
            shutdown_token.clone(),
            session,
            Arc::clone(&self.store),
            Arc::clone(&self.providers),
            Arc::clone(&self.tools),
            hooks,
            self.params.agent_paths.clone(),
            session_params,
            self.tx_event.clone(),
        ));

        SessionHandle {
            guide,
            pending,
            tx_inbound,
            tx_interrupt,
            tx_plugin,
            task,
            shutdown_token,
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
        // shutdown 同步快路径检查：已关闭立即拒绝（区分于 SessionNotFound）
        if self.shutdown.load(Ordering::Acquire) {
            return Err(EngineError::Shutdown);
        }

        let sessions = self.sessions.lock().await;
        let handle = sessions
            .get(id)
            .ok_or_else(|| EngineError::SessionNotFound(id.clone()))?;

        match event {
            InputEvent::User(user_msg) => {
                // 立即把 input 侧 UserMessage 字段照搬转化为 output 侧 UserMessage
                // （base + payload 完整保留，含 source），与 params 一起送进 session task。
                // 后续 handle_inbound_user 纯入队，inject_messages 消费时统一过管道。
                let outbound = fuyao_api::message::output::UserMessage {
                    base: user_msg.base,
                    payload: fuyao_api::message::output::UserPayload {
                        content: user_msg.payload.content,
                        mode: user_msg.payload.mode,
                        source: user_msg.payload.source,
                    },
                };
                let inbound = fuyao_api::InboundUser {
                    message: outbound,
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

    /// 销毁单个对话（动作五）
    ///
    /// 与 [`shutdown`](Self::shutdown) 对称但只针对一个 session：其他 session 不受影响。
    /// 适合 UI 形态需要"关闭某个对话但保留其他对话继续聊"的场景。
    ///
    /// 流程：
    /// 1. shutdown 快路径检查（引擎已关 → end 单 session 无意义，返 `Err(Shutdown)`）
    /// 2. 从调度表移除该 session 的 `SessionHandle`（不存在 → `Err(SessionNotFound)`）
    /// 3. cancel 该 session 的 **child_token**（**不动引擎 root token**，其他 session 不受影响）
    ///    → task 走与 shutdown 完全相同的优雅退出路径（idle select! / turn 中段 select! /
    ///    retry 退避 sleep 全部监听 child_token），退出前调 `store.update(session)` 落 in-flight 状态
    /// 4. 超时（`SHUTDOWN_TASK_TIMEOUT`）等待 task 退出，超时 `abort_handle.abort()` 兜底强杀
    /// 5. task 退出**之后**调 `store.end_session(id, reason)` 填 `ended_at` / `end_reason`
    ///    （时序关键：必须在 task 退出后调，否则会被 task 退出时的全量 `update(session)` 覆盖）
    ///
    /// 与 `shutdown` 的关系：二者共用私有 helper [`end_one_session`] 完成实际收尾，
    /// 差异只在"影响范围"——本方法动一个 child_token，shutdown 动引擎 root token + flag。
    ///
    /// **fire-and-forget task**（如标题生成）：不显式 abort，与 shutdown 一致——靠 runtime 关闭自然终止。
    ///
    /// # 错误
    /// - [`EngineError::Shutdown`]：引擎已 shutdown
    /// - [`EngineError::SessionNotFound`]：session id 不在活跃调度表（已结束或从未创建）
    /// - [`EngineError::Storage`]：落库 `ended_at` / `end_reason` 失败（task 已退出，但元数据未更新）
    pub async fn end_session(&self, id: &SessionId, end_reason: &str) -> Result<(), EngineError> {
        // shutdown 同步快路径检查：引擎已关 → end 单 session 无意义
        if self.shutdown.load(Ordering::Acquire) {
            return Err(EngineError::Shutdown);
        }

        // 从调度表移除（不存在 → SessionNotFound，区分于"引擎已关"）
        let handle = self
            .sessions
            .lock()
            .await
            .remove(id)
            .ok_or_else(|| EngineError::SessionNotFound(id.clone()))?;

        // cancel 该 session 的 **child_token**（不动 root，其他 session 不受影响）
        // → task 的 select! 收到 cancelled 走优雅退出路径（idle/turn/retry 三段都监听 child）
        handle.shutdown_token.cancel();

        // 收尾（超时 await → abort 兜底）
        let outcome = end_one_session(handle).await;
        match outcome {
            SessionExitOutcome::Finished => {
                tracing::info!(
                    session_id = %id,
                    end_reason = end_reason,
                    "session 已正常结束"
                );
            }
            SessionExitOutcome::Panicked(cause) => {
                tracing::warn!(
                    session_id = %id,
                    cause = %cause,
                    "session task panic 退出（已由 task 内 panic 防护或 runtime 兜底）"
                );
            }
            SessionExitOutcome::Aborted => {
                tracing::warn!(
                    session_id = %id,
                    timeout_secs = SHUTDOWN_TASK_TIMEOUT.as_secs(),
                    "session task 超时未退出，已强制 abort（兜底）"
                );
            }
        }

        // task 退出后再写 ended_at / end_reason，保证是最终值
        // （task 退出前的 store.update(session) 落的是当前元数据，ended_at/end_reason 仍为 None；
        //  这里的单字段 UPDATE 把它们写成最终值，不被覆盖）
        self.store.end_session(id, end_reason).await?;
        Ok(())
    }

    /// 关闭引擎
    ///
    /// 引擎关闭是危险操作，不混入对话级的事件流（不走 send），
    /// 由独立的关闭方法触发。
    ///
    /// 关闭流程（「显式关闭 + 等待退出 + 强制中止兜底」三层保障）：
    /// 1. shutdown flag 置位（`AtomicBool::store(true)`）→ 后续 `send` 立即返回 `Err(Shutdown)`，
    ///    `recv` 先 drain 残余事件再返回 None（不丢 shutdown 前最后几条产出）
    /// 2. cancel 引擎级 shutdown_token → 所有 session task 的 select! 同时收到 cancelled 信号
    /// 3. 每个 session task 优雅退出：select! 监听 cancelled → break 主循环 →
    ///    退出前调一次 `store.update(session)` 落库（保护 in-flight 状态，失败仅 warn 不阻塞）
    /// 4. **并发**收尾所有 task（每个 handle spawn 一个 [`end_one_session`] 进 JoinSet）：
    ///    每个 task 独立享 `SHUTDOWN_TASK_TIMEOUT` 超时预算，超时则 `abort_handle.abort()` 兜底强杀。
    ///    并发退出总耗时 ≈ `max(各 task 退出时间)`（JoinSet 同时调度），不受 session 数量影响
    /// 5. 发 INFO 日志（含正常退出 / panic / 超时 abort 计数）
    ///
    /// **并发等待的理由**：多 session 并发活跃时，各 task 的落库路径会竞争 SQLite WAL 写锁，
    /// 串行 await 会让总耗时退化成 `sum(各 task 退出时间)`；用 JoinSet 并发等待把总耗时压成
    /// `max(各 task 退出时间)`。
    ///
    /// **fire-and-forget task**（如 title 生成等 spawn 的独立 task）：**不显式 abort**，
    /// 靠 runtime 关闭自然终止（已记录决策：fire-and-forget task 不显式 abort）。
    pub async fn shutdown(&self) {
        // 1. flag 置位：后续 send / recv 立即走快路径拒绝
        self.shutdown.store(true, Ordering::Release);

        // 2. cancel 引擎级 token：所有 session task 的 child_token 同时 cancel
        self.shutdown_token.cancel();

        // 3. 取出所有 SessionHandle 的所有权（drain 出 hashmap 才能 move task 进 JoinSet）
        let handles: Vec<(SessionId, SessionHandle)> = {
            let mut sessions = self.sessions.lock().await;
            sessions.drain().collect()
        };

        let total = handles.len();
        if total == 0 {
            tracing::info!(total = 0, "引擎关闭完成（无活跃 session）");
            return;
        }

        // 4. 并发收尾：每个 handle spawn 一个 end_one_session
        //    （内含 cancel+超时+abort 兜底，与 end_session 单 session 版本共用同一份收尾逻辑）
        let mut set: JoinSet<(SessionId, SessionExitOutcome)> = JoinSet::new();
        for (id, handle) in handles {
            set.spawn(async move {
                let outcome = end_one_session(handle).await;
                (id, outcome)
            });
        }

        let mut finished = 0usize;
        let mut panicked = 0usize;
        let mut aborted_ids: Vec<SessionId> = Vec::new();

        // 收完所有结果（end_one_session 内部已各自有 SHUTDOWN_TASK_TIMEOUT 超时兜底，
        // 这里不再套外层 timeout——并发跑，总耗时 ≈ max(各 task 退出时间)）
        while let Some(joined) = set.join_next().await {
            let (id, outcome) = joined.expect("JoinSet task panic");
            match outcome {
                SessionExitOutcome::Finished => finished += 1,
                SessionExitOutcome::Panicked(cause) => {
                    panicked += 1;
                    tracing::warn!(
                        session_id = %id,
                        cause = %cause,
                        "session task panic 退出（已由 task 内 panic 防护或 runtime 兜底）"
                    );
                }
                SessionExitOutcome::Aborted => aborted_ids.push(id),
            }
        }

        if !aborted_ids.is_empty() {
            tracing::warn!(
                timeout_secs = SHUTDOWN_TASK_TIMEOUT.as_secs(),
                aborted_count = aborted_ids.len(),
                "部分 session task 超时未退出，已强制 abort（兜底）"
            );
        }

        tracing::info!(
            total,
            finished,
            panicked,
            aborted = aborted_ids.len(),
            "引擎关闭完成（所有 session task 已处理）"
        );
    }

    /// 出事件（单一出口）
    ///
    /// 从统一出口取下一条产出事件，按 session_id 归类到对应对话。
    /// 所有对话的产出都从此口流出，没有第二个出口。
    ///
    /// 返回 `None` 表示引擎已关闭、通道已断。
    ///
    /// shutdown 后调用：先 drain 残余事件（不丢 shutdown 前最后几条产出），
    /// 队列空了再返回 None——让消费者能完整收完 shutdown 前的事件流后优雅退出。
    pub async fn recv(&self) -> Option<OutputEvent> {
        // shutdown 后走快路径：drain 残余事件，再返回 None
        if self.shutdown.load(Ordering::Acquire) {
            return self.rx_event.lock().await.try_recv().ok();
        }
        self.rx_event.lock().await.recv().await
    }
}

/// session task 收尾后的退出结局
///
/// [`end_one_session`] 的返回值，[`Engine::end_session`] 与 [`Engine::shutdown`] 共用：
/// - `Finished`：task 正常退出（task 内 select! 收到 cancelled 后走中断路径落库退出）
/// - `Panicked`：task 以 panic 退出（payload 已转字符串，供 WARN 日志输出）
/// - `Aborted`：task 在 `SHUTDOWN_TASK_TIMEOUT` 内未退出，已 abort 强杀
enum SessionExitOutcome {
    Finished,
    Panicked(String),
    Aborted,
}

/// 单个 session task 的收尾（cancel + 超时等待 + abort 兜底）
///
/// `Engine::end_session`（单 session 销毁）与 `Engine::shutdown`（全部 session 销毁）
/// 共用本 helper，保证两条路径的收尾逻辑完全一致——差异只在"动哪个 token"：
/// - `end_session`：在调用方先 cancel 该 session 的 **child_token** 再调本 helper
///   （本 helper 不重复 cancel，避免与"child_token 已 cancel"假设耦合）
/// - `shutdown`：在调用方先 cancel 引擎级 **root token**，所有 child 同时 cancel，
///   然后把每个 handle spawn 进本 helper
///
/// 流程：
/// 1. 拿 `abort_handle`（独立于 `task` 的句柄，超时时用来 abort）
/// 2. `tokio::time::timeout(SHUTDOWN_TASK_TIMEOUT, handle.task)` 等 task 退出
///    - `Ok(Ok(()))`：task 正常完成 → `Finished`
///    - `Ok(Err(e))`：task panic → `Panicked(format_join_error(e))`
///    - `Err(_)`：超时 → `abort_handle.abort()` 强杀 + 等 abort 完成 → `Aborted`
///
/// 注：调用方必须保证进入本 helper 前 `handle.shutdown_token` 已被 cancel
/// （否则 task 可能永远不会退出，纯靠超时 abort 兜底）。本 helper 不自己 cancel
/// 是为了让"cancel 哪个 token"成为调用方决策（child vs root），逻辑更内聚。
async fn end_one_session(handle: SessionHandle) -> SessionExitOutcome {
    // 先拿独立 abort 句柄：超时分支需要它来强杀，而 handle.task 会被 timeout 消费
    let abort_handle = handle.task.abort_handle();

    match tokio::time::timeout(SHUTDOWN_TASK_TIMEOUT, handle.task).await {
        Ok(Ok(())) => SessionExitOutcome::Finished,
        Ok(Err(join_err)) => SessionExitOutcome::Panicked(format_join_error(join_err)),
        Err(_) => {
            // 超时强杀。abort 是异步的——发出信号后立即返回，task 实际终止由 runtime 调度。
            // 不在这里等 abort 完成（AbortHandle 不可 await）；调用方若需确保 task 已终止，
            // 可在更外层（如 Engine::shutdown 退出后 runtime drop）自然回收。
            abort_handle.abort();
            SessionExitOutcome::Aborted
        }
    }
}

/// 把 `JoinError` 格式化为可读字符串（用于日志）
///
/// panic 类型的任务退出原因通常含 payload，转字符串供 WARN 日志输出。
/// owned 传入：`try_into_panic` 消费 JoinError。
fn format_join_error(err: JoinError) -> String {
    if err.is_panic() {
        match err.try_into_panic() {
            Ok(payload) => fuyao_hooks::panic_payload_to_string(&*payload),
            Err(_) => "task panic（payload 不可恢复）".to_string(),
        }
    } else if err.is_cancelled() {
        "task 被取消".to_string()
    } else {
        format!("task 退出异常: {err}")
    }
}
