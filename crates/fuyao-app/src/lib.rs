//! Fuyao 应用装配入口
//!
//! 一键装配：初始化引擎 + 注册内置工具 + MCP 工具 + 内置插件。
//! 应用层（fuyao-cli / fuyao-tui）只需依赖 fuyao-app：
//! - [`start`]：一行启动，自动串联 `init_engine` + `setup`，返回
//!   `(Engine, EngineHandle, AppContext)`。
//! - [`init_engine`] + [`setup`]：分步装配，供需要介入中间过程的场景使用。

mod init;
mod logging;
mod mcp;
mod tools;

use std::sync::Arc;

use fuyao_core::EngineHandle;
use fuyao_guard::LoopGuardPlugin;
use fuyao_hooks::PluginHost;
use fuyao_mcp::MCPManager;
use fuyao_session::SessionPlugin;

pub use init::{InitError, init_engine};
pub use logging::LogGuard;

/// 装配产物：调用方持有，用于管理生命周期
pub struct AppContext {
    /// MCP 管理器（无配置 server 时为 None）
    pub mcp_manager: Option<Arc<MCPManager>>,
    /// 插件宿主（关闭时调用 dispose_all）
    pub plugin_host: PluginHost,
    /// 日志 guard：drop 时 flush 文件缓冲，须存活到引擎结束。
    ///
    /// `start` 路径下由 `init_engine` 初始化并注入；`init_engine + setup` 分步装配时
    /// 为空默认值（真正的 guard 在 `init_engine` 返回值里，由调用方自行持有）。
    pub log_guard: LogGuard,
}

/// 装配错误
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("缺少 AgentContext")]
    NoAgentContext,
    #[error(transparent)]
    /// 引擎初始化失败（配置加载、Provider 创建、模型校验等）
    Init(#[from] InitError),
    #[error(transparent)]
    /// 插件装配失败（重名等）
    PluginInstall(#[from] fuyao_hooks::PluginInstallError),
}

/// 一键启动：init_engine → setup，返回可直接使用的 `(Engine, EngineHandle, AppContext)`
///
/// 这是绝大多数应用推荐的入口：一行完成引擎装配 + 工具/MCP/插件注册 + 日志初始化。
/// 需要在两步之间介入（如动态注册工具）时，改用 [`init_engine`] + [`setup`] 分步装配。
pub async fn start(
    agent_ctx: fuyao_api::AgentContext,
) -> Result<(fuyao_core::Engine, EngineHandle, AppContext), SetupError> {
    let (engine, handle, log_guard) = init_engine(agent_ctx).map_err(|e| {
        tracing::error!(cause = %e, "引擎初始化失败");
        e
    })?;
    let mut app_ctx = setup(&handle).await.map_err(|e| {
        tracing::error!(cause = %e, "引擎装配失败 (setup)");
        e
    })?;
    app_ctx.log_guard = log_guard;
    Ok((engine, handle, app_ctx))
}

/// 一键装配：注册工具 + MCP + 内置插件（按 [plugins.enabled] 过滤）
///
/// 内部流程：
/// 1. 注册内置工具（fuyao-tools 静态表，按 [tools.enabled] 过滤）
/// 2. 注册 MCP 工具（MCPManager + start_all）
/// 3. 构造内置插件（按 [plugins.enabled] 过滤），装入 PluginHost
/// 4. host.install(&handle.hooks())
pub async fn setup(handle: &EngineHandle) -> Result<AppContext, SetupError> {
    // 1. 注册内置工具
    tools::register_builtin_tools(handle);
    tracing::info!(tools = handle.tools_schema().len(), "内置工具注册完成");

    // 2. 注册 MCP 工具
    let mcp_manager = mcp::register_mcp_tools(handle).await;

    // 3. 构造插件，按 [plugins.enabled] 过滤后装入
    let mut host = PluginHost::new();

    // session 插件需要 agent_paths（构造 SessionContext）+ agent_ctx（同步 session_id / 读 model_id）
    if is_plugin_enabled("session") {
        let agent_ctx = handle.agent_ctx().ok_or(SetupError::NoAgentContext)?;
        host.add(Box::new(SessionPlugin::new(
            agent_ctx.agent_paths.clone(),
            handle.agent_ctx_shared(),
        )));
    }

    if is_plugin_enabled("loop_guard") {
        host.add(Box::new(LoopGuardPlugin::new()));
    }

    // 4. 统一装配（重名插件返回 Err 使 setup 失败）
    host.install(&handle.hooks()).await?;
    tracing::info!(plugins = ?host.list(), "插件装配完成");

    Ok(AppContext {
        mcp_manager,
        plugin_host: host,
        log_guard: LogGuard::default(),
    })
}

/// 判断插件是否启用（[plugins.enabled] 中未列出或显式 true 均视为启用）
fn is_plugin_enabled(name: &str) -> bool {
    fuyao_api::get_config().plugins.enabled.get(name) != Some(&false)
}
