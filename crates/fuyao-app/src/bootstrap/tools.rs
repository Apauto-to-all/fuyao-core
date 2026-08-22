//! 工具装配：收集内置 + MCP 工具，汇总成引擎可注入的 `ToolRegistry`
//!
//! - [`collect_builtin_tools`]：`fuyao-tools` 静态表按 `[tools.enabled]` 过滤
//! - [`build_tool_registry`]：内置 + MCP 汇总 + 未知名对账，在 `Engine::new`
//!   之前调用，工具表一次性注入

use std::sync::Arc;

use fuyao_api::ToolEntry;
use fuyao_api::get_config;
use fuyao_core::{ToolRegistry, ToolRegistryBuilder};
use fuyao_mcp::MCPManager;

use super::mcp::collect_mcp_tools;

/// 收集内置工具（fuyao-tools 静态表），按 `[tools.enabled]` 过滤
///
/// 遍历 `fuyao-tools::all_tools()`，跳过被显式禁用的工具。
/// `ToolEntry` 已是 `fuyao-api` 共享类型，handler 为 `Arc` clone 廉价，直接 clone 收集。
pub fn collect_builtin_tools() -> Vec<ToolEntry> {
    let config = get_config();
    let mut entries = Vec::new();
    for (name, entry) in fuyao_tools::all_tools() {
        if config.tools.is_tool_disabled(name) {
            tracing::debug!(tool_name = *name, "工具被 [tools.enabled] 禁用，跳过");
            continue;
        }
        entries.push(entry.clone());
    }
    tracing::info!(tools = entries.len(), "内置工具收集完成");
    entries
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
    builder = builder.register_all(collect_builtin_tools());

    // 2. MCP 工具（有配置时启动 server 并收集）
    let mcp_manager = if let Some((manager, mcp_entries)) = collect_mcp_tools().await {
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
