//! 工具注册表
//!
//! 中心化工具注册表，所有工具在模块初始化时自动注册。
//! 每个工具提供 schema（JSON Schema 定义）和 handler（异步执行函数）。
//!
//! ## 设计
//!
//! 使用 `LazyLock<HashMap>` 实现编译时注册，零运行时开销。
//! Agent 通过 `get_tool()` 获取工具条目，通过 `all_tools()` 获取全部工具定义。

use fuyao_api::ToolEntry;
use std::collections::HashMap;
use std::sync::LazyLock;

/// 通用工具注册表
static TOOL_REGISTRY: LazyLock<HashMap<&'static str, ToolEntry>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    super::file::register(&mut map);
    super::skill::register(&mut map);
    super::subagent::register(&mut map);
    super::terminal::register(&mut map);
    super::todo::register(&mut map);
    super::web::register(&mut map);
    map
});

/// 获取所有已注册的通用工具
pub fn all_tools() -> &'static HashMap<&'static str, ToolEntry> {
    &TOOL_REGISTRY
}

/// 获取所有工具名称
pub fn all_tool_names() -> Vec<&'static str> {
    TOOL_REGISTRY.keys().copied().collect()
}

/// 获取指定工具
pub fn get_tool(name: &str) -> Option<&'static ToolEntry> {
    TOOL_REGISTRY.get(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_tools_returns_non_empty_registry() {
        let tools = all_tools();
        assert!(!tools.is_empty());
    }

    #[test]
    fn all_tool_names_returns_non_empty_list() {
        let names = all_tool_names();
        assert!(!names.is_empty());
    }

    #[test]
    fn get_tool_returns_none_for_unknown_name() {
        let result = get_tool("nonexistent_tool_xyz");
        assert!(result.is_none());
    }

    #[test]
    fn get_tool_returns_some_for_registered_tool() {
        // file 模块注册了 read 工具
        let result = get_tool("read");
        assert!(result.is_some());
    }

    #[test]
    fn get_tool_returns_correct_definition() {
        let entry = get_tool("read").unwrap();
        assert_eq!(entry.definition.function.name, "read");
    }

    #[test]
    fn all_registered_tools_have_valid_names() {
        let names = all_tool_names();
        for name in names {
            assert!(!name.is_empty());
            // 每个注册的工具都应该能通过 get_tool 获取
            assert!(get_tool(name).is_some());
        }
    }

    #[test]
    fn registered_tools_include_expected_tools() {
        let names = all_tool_names();
        // 应该包含这些核心工具
        assert!(names.contains(&"read"));
        assert!(names.contains(&"write"));
        assert!(names.contains(&"glob"));
        assert!(names.contains(&"grep"));
        assert!(names.contains(&"edit"));
        assert!(names.contains(&"skill"));
    }
}
