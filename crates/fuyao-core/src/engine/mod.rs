//! 引擎核心
//!
//! 两层分离的引擎层：启动一次，装配能力（provider / store / 出口通道）；
//! 多个对话按需创建，各自独立跑交互。
//!
//! 并发模型：每 session 一个独立 tokio task（异步并发）。
//! Engine 持调度表，各 task 并发跑，同 session 内单 task 串行。

pub(crate) mod types;

mod lifecycle;
mod runtime;
mod teardown;
#[cfg(test)]
mod tests;

use crate::engine::types::{SessionHandle, SharedQueue};
use crate::error::EngineError;
use crate::react;
use crate::tool_registry::ToolRegistry;
use fuyao_api::PluginEventSource;
use fuyao_api::message::output::{
    InterruptMessage as OutputInterruptMessage, PluginMessage as OutputPluginMessage,
};
use fuyao_api::{EngineParams, InputEvent, MessageKind, OutputEvent, Session, SessionParams};
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

/// 子任务 session 的上下文来源
///
/// [`Engine::create_child_session`] 用：决定新子任务 session 是空上下文起步，
/// 还是 fork 某个源 session 的可见上下文。两种模式产出的 session 都带 `parent_session_id`。
#[derive(Debug, Clone)]
pub enum ChildSessionSource {
    /// 全新创建：空上下文，`system_prompt` 从 `agent_config` 构建（与 `create_session` 一致）
    Fresh,
    /// fork 旧 session：复制 `source_id` 的可见消息 + `system_prompt`（复用 fork_session 的拷贝逻辑）
    Fork(SessionId),
}

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
}
