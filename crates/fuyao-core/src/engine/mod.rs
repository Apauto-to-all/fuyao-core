//! 引擎核心
//!
//! 两层分离的引擎层：启动一次，装配能力（provider / store / 工具 / 插件）；
//! 多个对话按需创建，各自独立跑交互。
//!
//! 并发模型：每 session 一个独立 tokio task（异步并发）。
//! Engine 持调度表，各 task 并发跑，同 session 内单 task 串行。
//!
//! 出站架构：**per-session 独立通道**——每个 session 持有自己的 `(tx, rx)` 通道对，
//! session task 的所有 emit 都进自己的 tx；rx 由调用方（装配层）取走消费。
//! Engine **不做 fan-in**——出站事件的汇聚是装配层的职责。

pub(crate) mod types;

mod lifecycle;
mod runtime;
mod stop;
mod subagent_ops;
mod teardown;
#[cfg(test)]
mod tests;

use crate::engine::types::{SessionHandle, SharedQueue, TurnPhase};
use crate::error::EngineError;
use crate::react;
use crate::tool_registry::ToolRegistry;
use fuyao_api::message::output::InterruptMessage as OutputInterruptMessage;
use fuyao_api::{
    ChildSessionSource, EngineParams, InputEvent, OutputEvent, Session, SessionParams,
};
use fuyao_hooks::{HooksRegistry, NamedPluginInstance, PluginHost, SessionSender, SharedHooks};
use fuyao_prompt::build_system_prompt;
use fuyao_provider::ProviderRegistry;
use fuyao_session::SessionStore;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
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
/// 作为「显式关闭 + 等待退出 + 强制中止兜底」三层保障中的总超时兜底。
const SHUTDOWN_TASK_TIMEOUT: Duration = Duration::from_secs(10);

/// 引擎
///
/// 能力共享层：构造时装配一次，持有 DB 句柄、provider、工具等引擎级共享。
/// 多个对话（Session）共享同一个 Engine 实例，靠 session id 区分。
///
/// 公开 API 遵循四个动作：
/// - [`new`](Self::new)：启动引擎（构造即启动）
/// - [`create_session`](Self::create_session)：创建对话（返 `(id, rx)`，rx 由调用方消费）
/// - [`resume_session`](Self::resume_session)：恢复对话（返 `(id, rx)`）
/// - [`send`](Self::send)：入事件（单一入口）
/// - [`shutdown`](Self::shutdown)：关闭引擎（独立方法，不走消息流）
///
/// **没有 `recv` 方法**——出站事件靠每 session 自己的 rx 消费（per-session 通道化）。
/// 装配层负责把多个 session 的 rx fan-in 成单一出口。
pub struct Engine {
    /// 会话存储层（Arc 共享给各 session task）
    store: Arc<SessionStore>,

    /// Provider 实例注册表（多 Provider 路由）
    ///
    /// 引擎级共享：持有所有已注册 Provider 的构造好的实例（按 provider_id 索引）。
    /// session 的 `SessionParams.model_config.model_id` 形如 `"provider_id/model_id"`——
    /// session task 在执行该轮 LLM 调用前，从 model_id 拆出 provider_id，
    /// 从本注册表取对应 Provider 实例，实现"不同 session 用不同 Provider"。
    /// model_config 整 session 共享一份，可经 `update_session_params` 随时切（下一轮生效）。
    ///
    /// 与旧"引擎持单个 `Arc<dyn Provider>`"模型的差异：旧模型启动时按 default
    /// model_id 选一个 Provider，所有调用都打到这里；新模型按 session 的 provider_id 路由。
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

    /// 引擎启动参数（引擎级，含 agent_paths 等，后续可拓展）
    params: EngineParams,

    /// 引擎是否已 shutdown（AtomicBool 同步快路径）
    ///
    /// shutdown 后置 true，作为 `send` 的同步快路径检查：
    /// - `send` 立即返回 `Err(EngineError::Shutdown)`（区分于 `SessionNotFound`）
    ///
    /// 用 AtomicBool 而非 CancellationToken：send 入口检查需同步、不 await，
    /// AtomicBool 满足零开销同步语义；task 内的取消信号另用 `shutdown_token`。
    shutdown: Arc<AtomicBool>,

    /// 引擎级关闭信号（cancel 后所有 session task 的 select! 同时收到）
    ///
    /// 每个 session 在 `assemble_session` 时用 `child_token()` 派生子 token，
    /// 既支持引擎级一次 cancel 全部（Engine::shutdown 调 root.cancel），
    /// 也为未来「单 session 销毁」扩展点（cancel 单个 child）预留。
    shutdown_token: CancellationToken,

    /// 引擎自身的弱引用（`Arc::new_cyclic` 构造时注入）
    ///
    /// 用于在不破坏构造顺序（ToolRegistry 先于 Engine 装配）的前提下，
    /// 把引擎能力以弱引用形式注入工具调用上下文（[`crate::tool_exec`]），
    /// 供子代理类工具派生子 session 时 upgrade 后调用。
    /// 强引用循环避免：Engine → ToolRegistry → handler → ctx → Weak<Engine> 不成强环。
    engine_weak: Weak<Engine>,
}

impl Engine {
    /// 启动引擎（动作一）
    ///
    /// 构造即启动：装配 provider、工具注册表，注入会话存储。
    /// 启动完成后才可创建/恢复对话。
    ///
    /// 返回 `Arc<Engine>`——用 `Arc::new_cyclic` 构造，让引擎拿到自身的弱引用，
    /// 存入 `engine_weak` 字段，供后续注入工具调用上下文（子代理工具派生子 session）。
    ///
    /// `tools` 由装配方注入（如从 `fuyao_tools::all_tools()` 转换），引擎持有后
    /// 所有 session task 共享同一份工具表。
    ///
    /// `providers` 是 Provider 实例注册表（多 Provider 路由）：启动时由装配方从
    /// 已注册 Provider 配置批量构造实例（`ProviderRegistry::from_registered`），
    /// session 的 `SessionParams.model_config.model_id` 决定本轮走哪个 Provider。
    ///
    /// `plugin_host` 是插件工厂集合，引擎级共享。每个 session 启动时调用
    /// [`PluginHost::create_instances`] 生成该 session 的独立实例，
    /// 各实例 register 到该 session 私有的 HooksRegistry。拦截/观察在 session task 内执行。
    ///
    /// `store` 是会话存储句柄，由装配方创建并注入。Engine 是 store 的消费者
    /// （LLM 流程落库），不是创建者——存储的所有权归装配层，便于查询门面
    /// （如 [`SessionManager`](../../fuyao_app/session_manager/struct.SessionManager.html)）
    /// 共享同一份 store。注入的 `Arc` 与其他消费者共享同一连接池。
    ///
    /// **不建立出口通道**——per-session 出站通道在 [`Engine::assemble_session`]
    /// 时按 session 独立创建，rx 随创建方法返回给调用方。
    pub async fn new(
        params: EngineParams,
        providers: ProviderRegistry,
        tools: ToolRegistry,
        plugin_host: PluginHost,
        store: Arc<SessionStore>,
    ) -> Arc<Self> {
        // Arc::new_cyclic：构造 Engine 时拿到自身的 Weak 引用，
        // 存入 engine_weak 字段供后续注入工具 ctx（子代理工具用）
        Arc::new_cyclic(|weak| Engine {
            store,
            providers: Arc::new(providers),
            tools: Arc::new(tools),
            plugin_host: Arc::new(plugin_host),
            sessions: Mutex::new(std::collections::HashMap::new()),
            params,
            shutdown: Arc::new(AtomicBool::new(false)),
            shutdown_token: CancellationToken::new(),
            engine_weak: weak.clone(),
        })
    }

    /// 引擎自身的弱引用（trait object 形式，供工具调用上下文注入）
    ///
    /// 返回 `Weak<dyn SubagentOps>`——子代理类工具 handler upgrade 后调
    /// [`SubagentOps`] 方法派生子 session。普通工具忽略此字段。
    pub(crate) fn subagent_ops_weak(&self) -> Weak<dyn fuyao_api::SubagentOps> {
        // unsized coerce: Weak<Engine> → Weak<dyn SubagentOps>
        // （Engine impl SubagentOps 见 engine/subagent_ops.rs）
        self.engine_weak.clone()
    }
}
