//! Fuyao 应用装配入口
//!
//! 一键装配：初始化（配置 / 日志 / Provider）+ 收集工具（内置 + MCP）+ 启动引擎。
//! 应用层（fuyao-cli / fuyao-tui）只需依赖 fuyao-app：
//! - [`start`]：一行启动，串联 `init_engine` → `build_tool_registry` → `Engine::new`，
//!   返回可直接使用的 `(Engine, AppContext)`。
//! - [`init_engine`] + [`build_tool_registry`]：分步装配，供需要介入中间过程的场景使用。
//!
//! 工具注入时机：新架构无事后注册的 EngineHandle，工具必须在 `Engine::new` 前收集成
//! `ToolRegistry` 一次性注入（见设计文档「动作一·启动引擎」）。

mod init;
mod logging;
mod mcp;
mod tools;

use std::sync::Arc;

use fuyao_api::{AgentPaths, EngineParams};
use fuyao_core::{Engine, SharedHooks, ToolRegistry, ToolRegistryBuilder};
use fuyao_hooks::HooksRegistry;
use fuyao_mcp::MCPManager;

pub use init::{InitError, InitResult, init_engine};
pub use logging::LogGuard;

/// 装配产物：调用方持有，用于管理生命周期
pub struct AppContext {
    /// MCP 管理器（无配置 server 时为 None）。调用方须持有保活，否则 MCP 工具失效。
    pub mcp_manager: Option<Arc<MCPManager>>,
    /// 日志 guard：drop 时 flush 文件缓冲，须存活到引擎结束。
    pub log_guard: LogGuard,
    /// 推断出的默认 model_id（消息级缺省时兜底用）
    pub default_model_id: String,
}

/// 装配错误
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error(transparent)]
    /// 引擎装配准备失败（配置加载、Provider 创建、模型校验等）
    Init(#[from] InitError),
}

/// 一键启动：init_engine → build_tool_registry → Engine::new
///
/// 这是绝大多数应用推荐的入口：一行完成配置/日志/Provider 准备 +
/// 工具收集（内置 + MCP）+ 引擎启动，返回可直接使用的 `(Engine, AppContext)`。
///
/// 需要在中间介入（如动态追加工具）时，改用 [`init_engine`] + [`build_tool_registry`]
/// 分步装配，再自行调 `Engine::new`。
pub async fn start(agent_paths: AgentPaths) -> Result<(Engine, AppContext), SetupError> {
    // 1. 配置 / 日志 / Provider 准备
    let init::InitResult {
        provider,
        default_model_id,
        log_guard,
    } = init_engine(agent_paths.clone()).await.map_err(|e| {
        tracing::error!(cause = %e, "引擎装配准备失败");
        e
    })?;

    // 2. 收集工具 + 启动 MCP（内置 + MCP 汇总进 ToolRegistry）
    let (tools, mcp_manager) = build_tool_registry().await;

    // 3. 构造钩子注册表（本轮不装配插件，传空注册表；机制已接通）
    //    后续在此处 PluginHost::install 注册插件后再传入。
    let hooks: SharedHooks = std::sync::Arc::new(tokio::sync::Mutex::new(HooksRegistry::default()));

    // 4. 启动引擎（工具 + 钩子构造时注入）
    let engine = Engine::new(EngineParams { agent_paths }, provider, tools, hooks).await;

    tracing::info!(model_id = %default_model_id, "引擎启动完成");

    Ok((
        engine,
        AppContext {
            mcp_manager,
            log_guard,
            default_model_id,
        },
    ))
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

    (builder.build(), mcp_manager)
}
