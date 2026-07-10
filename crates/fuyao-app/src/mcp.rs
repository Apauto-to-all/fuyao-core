//! MCP 工具注册

use std::sync::Arc;

use fuyao_core::EngineHandle;
use fuyao_mcp::MCPManager;

/// 注册 MCP 工具到 EngineHandle
///
/// 从 `[mcp_servers]` 配置创建 MCPManager，启动所有 server 连接，
/// 将发现的工具注册到 EngineHandle。
///
/// 无配置 server 时返回 None。
pub async fn register_mcp_tools(handle: &EngineHandle) -> Option<Arc<MCPManager>> {
    if fuyao_api::get_config().mcp_servers.is_empty() {
        return None;
    }

    let manager = Arc::new(MCPManager::from_config());
    let (success, fail_count, failures) = manager.start_all().await;

    if fail_count > 0 {
        tracing::warn!(
            failed = fail_count,
            total = success + fail_count,
            servers = ?failures.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            "MCP server 启动部分失败"
        );
    }

    for (name, schema, handler) in manager.get_tool_entries().await {
        handle.register_tool(&name, schema, handler);
    }

    Some(manager)
}
