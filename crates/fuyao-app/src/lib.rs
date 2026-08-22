//! Fuyao 应用装配入口
//!
//! 一键装配：初始化（配置 / 日志 / Provider）+ 收集工具（内置 + MCP）+ 启动引擎。
//! 应用层（fuyao-cli / fuyao-tui）只需依赖 fuyao-app：
//! - [`start`]：一行启动，串联 `init_engine` → `build_tool_registry` →
//!   `Engine::new` → [`App`] 装配（fan-in 单一出口），返回可直接使用的 [`App`]。
//! - [`App`]：装配产物，包装 [`Engine`] + fan-in 出口（[`App::recv`]）。
//! - [`init_engine`] + [`build_tool_registry`]：分步装配，供需要介入中间过程的场景使用。
//! - [`list_agent_ids`]：启动前列举可选 agent_id（传入 `AgentPaths`，不依赖引擎）。
//!
//! 工具注入时机：新架构无事后注册的 EngineHandle，工具必须在 `Engine::new` 前收集成
//! `ToolRegistry` 一次性注入（启动引擎时装配）。

mod bootstrap;
mod manager;
mod runtime;

use std::sync::Arc;

use fuyao_api::EngineParams;
use fuyao_core::{Engine, PluginHost};
use fuyao_session::SessionStore;

pub use bootstrap::{InitError, InitResult, LogGuard, build_tool_registry, init_engine};
pub use manager::list_agent_ids;
pub use manager::{ProviderAdminError, ProviderManager, ProviderModelSpec, ProviderSpec};
pub use runtime::{App, Discovery, SessionManager};
// 透出 fuyao-api 的列举选项类型，二次开发只依赖 fuyao-app 即可消费列举结果
pub use fuyao_api::{
    AgentIdOption, AgentIdSource, DefinitionOption, ProviderModelOption, ProviderOption,
};

/// 装配错误
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error(transparent)]
    /// 引擎装配准备失败（配置加载、Provider 创建、模型校验等）
    Init(#[from] InitError),

    /// 会话存储初始化失败（打开数据库、建表等）
    #[error("会话存储初始化失败: {0}")]
    Storage(String),
}

/// 装配产物：运行时交互入口 + 会话管理入口 + 选择支持入口
///
/// [`start`] 一键装配后返回本聚合体，上层（cli / tui）同时拿到三个正交门面：
/// - [`app`](self::FuyaoApp::app)：运行时交互（create / send / recv / 对话生命周期）
/// - [`sessions`](self::FuyaoApp::sessions)：会话管理查询（列会话 / 查历史）
/// - [`discovery`](self::FuyaoApp::discovery)：选择支持（列 agent_id / Agent 定义）
///
/// 前两者共享同一份 `SessionStore`（store 所有权归装配层，Engine 与 SessionManager
/// 各持一份 `Arc` 克隆，零拷贝共享连接池）；[`discovery`] 仅凭路径构造，
/// 不依赖引擎运行时。
pub struct FuyaoApp {
    /// 运行时交互门面（对话的进行）
    pub app: App,
    /// 会话管理门面（会话的检索与浏览）
    pub sessions: SessionManager,
    /// 选择支持门面（列举 agent_id / Agent 定义）
    pub discovery: Discovery,
}

/// 一键启动：init_engine → build_tool_registry → 创建 store → Engine::new → 装配
///
/// 这是绝大多数应用推荐的入口：一行完成配置/日志/Provider 准备 +
/// 工具收集（内置 + MCP）+ 会话存储创建 + 引擎启动 + 装配，返回可直接使用的 [`FuyaoApp`]。
///
/// 与早期只返单个 [`App`] 的差异：store 所有权上移到装配层——Engine 不再内部创建 store，
/// 而是由本函数创建后注入 Engine 与 [`SessionManager`]，两者共享同一份连接池。
///
/// 需要在中间介入（如动态追加工具）时，改用 [`init_engine`] + [`build_tool_registry`]
/// 分步装配，再自行创建 store、调 `Engine::new` + [`App::new`] + [`SessionManager::new`]。
pub async fn start(params: EngineParams) -> Result<FuyaoApp, SetupError> {
    // 1. 配置 / 日志 / Provider 准备（init_engine 内部取出 agent_paths 供子流程定位路径）
    let InitResult {
        provider,
        log_guard,
    } = init_engine(&params).await.map_err(|e| {
        tracing::error!(cause = %e, "引擎装配准备失败");
        e
    })?;

    // 2. 收集工具 + 启动 MCP（内置 + MCP 汇总进 ToolRegistry）
    let (tools, mcp_manager) = build_tool_registry().await;

    // 3. 装配插件工厂（per-session 实例化的引擎级入口）
    //    每个 session 启动时由 Engine 调 create_instances 生成独立实例，
    //    多 session 并发时各插件状态互不串台。
    let mut plugin_host = PluginHost::new();
    plugin_host.add(Box::new(fuyao_guard::LoopGuardPlugin::new()));

    // 4. 创建会话存储（所有权归装配层，注入 Engine 与 SessionManager 共享）
    //    store 由装配层创建，Engine 与 SessionManager 各持 Arc 克隆，共享同一连接池。
    let db_path = params.agent_paths.sessions_db_path();
    let store = Arc::new(
        SessionStore::new(db_path)
            .await
            .map_err(|e| SetupError::Storage(e.to_string()))?,
    );

    // 5. 构造选择支持门面（持有启动时的完整路径身份，含 agent_id）
    //    在 Engine::new 消费 params 前 clone 出 agent_paths，供其零参数查询复用。
    let discovery = Discovery::new(params.agent_paths.clone());

    // 6. 启动引擎（store 注入，工具 + 插件工厂构造时注入）
    //    重试在 session 内由 RetryRunner 驱动（per-session，发 OutputEvent::Retry）
    let engine = Engine::new(params, provider, tools, plugin_host, store.clone()).await;

    tracing::info!("引擎启动完成");

    // 7. 装配产物：运行时交互门面 + 会话管理门面（共享同一份 store）+ 选择支持门面
    Ok(FuyaoApp {
        app: App::new(engine, mcp_manager, log_guard),
        sessions: SessionManager::new(store),
        discovery,
    })
}
