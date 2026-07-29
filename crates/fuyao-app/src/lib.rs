//! Fuyao 应用装配入口
//!
//! 一键装配：初始化（配置 / 日志 / Provider）+ 收集工具（内置 + MCP）+ 启动引擎。
//! 应用层（fuyao-cli / fuyao-tui）只需依赖 fuyao-app：
//! - [`start`]：一行启动，串联 `init_engine` → `build_tool_registry` → `Engine::new` →
//!   [`App`] 装配（fan-in 单一出口），返回可直接使用的 [`App`]。
//! - [`App`]：装配产物，包装 [`Engine`] + fan-in 出口（[`App::recv`]）。
//! - [`init_engine`] + [`build_tool_registry`]：分步装配，供需要介入中间过程的场景使用。
//!
//! 工具注入时机：新架构无事后注册的 EngineHandle，工具必须在 `Engine::new` 前收集成
//! `ToolRegistry` 一次性注入（启动引擎时装配）。

mod app;
mod init;
mod logging;
mod mcp;
mod tools;

use std::sync::Arc;

use fuyao_api::EngineParams;
use fuyao_core::{Engine, PluginHost, ToolRegistry, ToolRegistryBuilder};
use fuyao_mcp::MCPManager;

pub use app::App;
pub use init::{InitError, InitResult, init_engine};
pub use logging::LogGuard;

/// 装配错误
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error(transparent)]
    /// 引擎装配准备失败（配置加载、Provider 创建、模型校验等）
    Init(#[from] InitError),
}

/// 一键启动：init_engine → build_tool_registry → Engine::new → App 装配
///
/// 这是绝大多数应用推荐的入口：一行完成配置/日志/Provider 准备 +
/// 工具收集（内置 + MCP）+ 引擎启动 + fan-in 装配，返回可直接使用的 [`App`]。
///
/// 需要在中间介入（如动态追加工具）时，改用 [`init_engine`] + [`build_tool_registry`]
/// 分步装配，再自行调 `Engine::new` + [`App::new`]。
pub async fn start(params: EngineParams) -> Result<App, SetupError> {
    // 1. 配置 / 日志 / Provider 准备（init_engine 内部取出 agent_paths 供子流程定位路径）
    let init::InitResult {
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

    // 4. 启动引擎（工具 + 插件工厂构造时注入）
    //    重试在 session 内由 RetryRunner 驱动（per-session，发 OutputEvent::Retry）
    let engine = Engine::new(params, provider, tools, plugin_host).await;

    tracing::info!("引擎启动完成");

    // 5. 装配 App（fan-in 单一出口）
    Ok(App::new(engine, mcp_manager, log_guard))
}

/// 收集工具（内置 + MCP），汇总成引擎可注入的 `ToolRegistry`
///
/// - 内置工具：`fuyao-tools` 静态表，按 `[tools.enabled]` 过滤。
/// - MCP 工具：`[mcp_servers]` 配置驱动，无配置时跳过（返回的 manager 为 None）。
///
/// 返回 `(ToolRegistry, MCPManager)`。调用方持有 MCPManager 保活，否则底层
/// server 连接断开、MCP 工具 handler 失效。
pub async fn build_tool_registry() -> (ToolRegistry, Option<Arc<MCPManager>>) {
    let mut builder = ToolRegistryBuilder::default();

    // 1. 内置工具
    builder = builder.register_all(tools::collect_builtin_tools());

    // 2. MCP 工具（有配置时启动 server 并收集）
    let mcp_manager = if let Some((manager, mcp_entries)) = mcp::collect_mcp_tools().await {
        builder = builder.register_all(mcp_entries);
        Some(manager)
    } else {
        None
    };

    let registry = builder.build();

    // 全局层未知名对账（与定义层 tools 同款逻辑：静默忽略 + WARN，用户错误用户承担）。
    // 已知名取注册表全部（内置 + MCP），避免已配置的 MCP 工具名被误报未知。
    for name in
        fuyao_api::unknown_tool_names(&fuyao_api::get_config().tools.enabled, registry.names())
    {
        tracing::warn!(
            tool_name = %name,
            layer = "global",
            "工具配置引用了未知的工具名，已忽略"
        );
    }

    (registry, mcp_manager)
}
