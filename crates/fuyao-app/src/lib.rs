//! Fuyao 应用装配入口
//!
//! 一键装配：注册内置工具 + MCP 工具 + 内置插件，返回 [`AppContext`]。
//! 应用层（fuyao-cli / fuyao-tui）只需依赖 fuyao-app，一行 [`setup`] 完成装配。

mod mcp;
mod tools;

use std::sync::Arc;

use fuyao_core::EngineHandle;
use fuyao_guard::LoopGuardPlugin;
use fuyao_hooks::PluginHost;
use fuyao_mcp::MCPManager;
use fuyao_session::SessionPlugin;

/// 装配产物：调用方持有，用于管理生命周期
pub struct AppContext {
    /// MCP 管理器（无配置 server 时为 None）
    pub mcp_manager: Option<Arc<MCPManager>>,
    /// 插件宿主（关闭时调用 dispose_all）
    pub plugin_host: PluginHost,
}

/// 装配错误
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("缺少 AgentContext")]
    NoAgentContext,
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

    // 4. 统一装配
    host.install(&handle.hooks()).await;

    Ok(AppContext {
        mcp_manager,
        plugin_host: host,
    })
}

/// 判断插件是否启用（[plugins.enabled] 中未列出或显式 true 均视为启用）
fn is_plugin_enabled(name: &str) -> bool {
    fuyao_api::get_config().plugins.enabled.get(name) != Some(&false)
}
