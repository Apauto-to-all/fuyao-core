//! 工具条目：schema 定义 + 执行 handler + 可见性元数据
//!
//! 作为工具系统各参与方（fuyao-tools 的静态注册表、fuyao-core 的引擎注册表、
//! fuyao-app 的装配）共享的唯一类型，消除此前两份字段同型结构体间的逐字段转换。

use std::collections::HashMap;

use crate::tool::{ToolDefinition, ToolFn};

/// 工具条目：schema 定义 + 执行 handler + 可见性元数据
///
/// handler 是 `Arc`，clone 廉价，多 session 共享同一份函数指针。
#[derive(Clone)]
pub struct ToolEntry {
    /// 工具的 JSON Schema 定义（序列化后发给 LLM）
    pub definition: ToolDefinition,
    /// 工具执行函数（接收 args + 上下文，返回统一结果信封）
    pub handler: ToolFn,
    /// 是否对子 session 隐藏（递归防护）
    ///
    /// `true` 时该工具不出现在子任务 session 的工具列表里——LLM 看不到就不会调，
    /// 阻断子代理嵌套派生。默认 `false`（普通工具主子 session 都可见）；
    /// 派生类工具（如子代理工具）标 `true`。
    pub child_invisible: bool,
}

impl ToolEntry {
    /// 组装工具条目（schema 定义 + 规范签名 handler + 可见性）
    ///
    /// handler 通常由 [`crate::tool_handler`] 包装规范签名异步函数得到。
    pub fn new(definition: ToolDefinition, handler: ToolFn, child_invisible: bool) -> Self {
        Self {
            definition,
            handler,
            child_invisible,
        }
    }

    /// 工具名（单一来源：schema 定义里的 `function.name`，与 LLM 可见名一致）
    pub fn name(&self) -> &str {
        &self.definition.function.name
    }
}

/// 以 schema 名为 key 注册工具条目进注册表
///
/// 工具名只认 schema 定义里的 `function.name`（单一来源，不在注册处重复书写）。
/// 静态注册表在模块初始化期构建，重名属于编程错误，立即 panic 暴露。
pub fn insert_tool(map: &mut HashMap<String, ToolEntry>, entry: ToolEntry) {
    let name = entry.name().to_string();
    if map.insert(name.clone(), entry).is_some() {
        panic!("工具名重复注册: {name}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_handler;
    use crate::{CancellationToken, ToolCallContext, ToolOutput};

    async fn noop(
        _args: serde_json::Value,
        _ctx: ToolCallContext,
        _cancel: CancellationToken,
    ) -> ToolOutput {
        ToolOutput::text("ok")
    }

    fn sample_entry(name: &str) -> ToolEntry {
        ToolEntry::new(
            ToolDefinition::builder(name, "示例工具").build(),
            tool_handler(noop),
            false,
        )
    }

    #[test]
    fn name_comes_from_schema_definition() {
        let entry = sample_entry("read");
        assert_eq!(entry.name(), "read");
    }

    #[test]
    fn insert_tool_keys_by_schema_name() {
        let mut map = HashMap::new();
        insert_tool(&mut map, sample_entry("read"));
        assert!(map.contains_key("read"));
    }

    #[test]
    #[should_panic(expected = "工具名重复注册")]
    fn insert_tool_panics_on_duplicate_name() {
        let mut map = HashMap::new();
        insert_tool(&mut map, sample_entry("read"));
        insert_tool(&mut map, sample_entry("read"));
    }
}
