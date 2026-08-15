//! MCP 工具收集
//!
//! 从 `[mcp_servers]` 配置启动 MCP server，把发现的工具收集成 `fuyao_api::ToolEntry`。
//! 返回 MCPManager（调用方持有保活，否则连接断开）+ 工具列表。
//!
//! 工具收集发生在 `Engine::new` 之前——MCP 工具与内置工具一起注入 `ToolRegistry`。

use std::sync::Arc;

use fuyao_api::ToolEntry;
use fuyao_api::get_config;
use fuyao_mcp::MCPManager;

/// 收集 MCP 工具
///
/// 从 `[mcp_servers]` 配置创建 MCPManager，启动所有 server 连接，
/// 收集已发现工具的注册条目（强类型 schema + bridge 生成的 handler）。
///
/// - 无配置 server → 返回 `None`（不启动子进程）。
/// - 部分启动失败 → 仅记录 WARN，继续收集已成功的工具。
///
/// 返回 `(MCPManager, 工具列表)`：调用方须持有 MCPManager 保活，
/// 否则底层 server 连接断开，工具 handler 会失效。
pub async fn collect_mcp_tools() -> Option<(Arc<MCPManager>, Vec<ToolEntry>)> {
    if get_config().mcp_servers.is_empty() {
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

    let entries = manager.get_tool_entries().await;

    tracing::info!(tools = entries.len(), "MCP 工具收集完成");
    Some((manager, entries))
}
