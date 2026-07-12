//! 内置工具注册

use fuyao_core::EngineHandle;

/// 注册内置工具到 EngineHandle
///
/// 遍历 fuyao-tools 静态工具表，逐一调用 `register_tool`。
/// 工具开关过滤已下沉到 `EngineHandle::register_tool` 漏斗统一处理
/// （覆盖内置 / MCP / 插件所有来源），此处不再重复过滤。
pub fn register_builtin_tools(handle: &EngineHandle) {
    for (name, entry) in fuyao_tools::all_tools() {
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
