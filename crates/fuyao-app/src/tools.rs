//! 内置工具收集
//!
//! 把 `fuyao-tools` 静态表里的工具，转换成引擎可注入的 `fuyao_core::ToolEntry`，
//! 按 `[tools.enabled]` 过滤禁用项。返回的工具列表由 [`crate::build_tool_registry`]
//! 汇总进 `ToolRegistry`，在 `Engine::new` 时一次性注入。

use fuyao_api::get_config;
use fuyao_core::ToolEntry;

/// 收集内置工具（fuyao-tools 静态表），按 `[tools.enabled]` 过滤
///
/// 遍历 `fuyao-tools::all_tools()`，跳过被显式禁用的工具，
/// 把每个条目从 `fuyao_tools::ToolEntry` 转换成 `fuyao_core::ToolEntry`（字段同型，
/// 都是来自 fuyao-api 的 `ToolDefinition` + `ToolFn`，handler 是 `Arc` clone 廉价）。
pub fn collect_builtin_tools() -> Vec<ToolEntry> {
    let config = get_config();
    let mut entries = Vec::new();
    for (name, entry) in fuyao_tools::all_tools() {
        if config.tools.is_tool_disabled(name) {
            tracing::debug!(tool_name = *name, "工具被 [tools.enabled] 禁用，跳过");
            continue;
        }
        entries.push(ToolEntry {
            definition: entry.definition.clone(),
            handler: entry.handler.clone(),
            child_invisible: entry.child_invisible,
        });
    }
    tracing::info!(tools = entries.len(), "内置工具收集完成");
    entries
}
