//! 内置工具注册

use fuyao_core::EngineHandle;

/// 注册内置工具到 EngineHandle
///
/// 遍历 fuyao-tools 静态工具表，按 `[tools.enabled]` 过滤：
/// - 显式 `false`：跳过
/// - 未列出或 `true`：注册
pub fn register_builtin_tools(handle: &EngineHandle) {
    let enabled = &fuyao_api::get_config().tools.enabled;
    for (name, entry) in fuyao_tools::all_tools() {
        if enabled.get(*name) == Some(&false) {
            continue;
        }
        let schema = serde_json::to_value(&entry.definition).unwrap_or_else(|_| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": *name,
                    "description": "",
                    "parameters": { "type": "object", "properties": {} }
                }
            })
        });
        handle.register_tool(name, schema, entry.handler.clone());
    }
}
