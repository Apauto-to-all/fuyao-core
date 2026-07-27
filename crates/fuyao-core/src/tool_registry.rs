//! 引擎级工具注册表
//!
//! 工具注册是引擎级共享能力：启动时装配一次，所有 session task 共用同一份注册表。
//! 工具执行则是 session 级（各 task 各跑各的），见 [`crate::tool_exec`]。
//!
//! 设计上不依赖 `fuyao-tools` crate——装配方（如 `fuyao-app`）负责从任意来源
//! （内置工具、MCP 工具、外部注册的工具）收集 [`ToolEntry`] 注入进来。
//! 这样核心保持轻量，同时支持未来动态注册外部工具。

use fuyao_api::{ToolDefinition, ToolFn};
use std::collections::HashMap;

/// 工具条目：schema 定义 + 执行 handler + 可见性元数据
///
/// handler 是 `Arc`，clone 廉价，多 session 共享同一份函数指针。
#[derive(Clone)]
pub struct ToolEntry {
    /// 工具的 JSON Schema 定义（序列化后发给 LLM）
    pub definition: ToolDefinition,
    /// 工具执行函数（接收 args + 上下文，返回结果字符串）
    pub handler: ToolFn,
    /// 是否对子 session 隐藏（递归防护）
    ///
    /// `true` 时该工具不出现在子任务 session 的工具列表里——LLM 看不到就不会调，
    /// 阻断子代理嵌套派生。默认 `false`（普通工具主子 session 都可见）；
    /// 派生类工具（如子代理工具）标 `true`。
    pub child_invisible: bool,
}

/// 工具注册表（引擎级共享）
///
/// 启动时由装配方通过 [`ToolRegistryBuilder`] 装配，引擎持有 `Arc<ToolRegistry>`
/// 共享给所有 session task。运行时只读——task 通过 [`get`](Self::get) 查 handler 执行。
pub struct ToolRegistry {
    inner: HashMap<String, ToolEntry>,
}

impl ToolRegistry {
    /// 创建空注册表的构建器
    pub fn builder() -> ToolRegistryBuilder {
        ToolRegistryBuilder::default()
    }

    /// 按名称查工具
    pub fn get(&self, name: &str) -> Option<&ToolEntry> {
        self.inner.get(name)
    }

    /// 是否为空（无工具注册）
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// 已注册工具数量
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// 序列化工具定义为 JSON Value 数组，按当前 session 是否子任务过滤
    ///
    /// `is_child=true` 时跳过 [`ToolEntry::child_invisible`] 为 `true` 的工具
    /// （递归防护：子 session 看不到派生类工具，LLM 不会尝试调用，根本性阻断嵌套派生）。
    ///
    /// `ToolDefinition` 已实现 `Serialize`，直接 `to_value` 即可。
    /// 空注册表返回空 Vec（调用方据此决定是否带 tools 字段）。
    pub fn definitions_json_for(&self, is_child: bool) -> Vec<serde_json::Value> {
        self.inner
            .values()
            .filter(|e| !is_child || !e.child_invisible)
            .filter_map(|e| serde_json::to_value(&e.definition).ok())
            .collect()
    }
}

/// 工具注册表构建器
///
/// 装配方链式注册工具后调 [`build`](Self::build) 生成不可变 [`ToolRegistry`]。
#[derive(Default)]
pub struct ToolRegistryBuilder {
    inner: HashMap<String, ToolEntry>,
}

impl ToolRegistryBuilder {
    /// 注册一个工具
    pub fn register(mut self, entry: ToolEntry) -> Self {
        let name = entry.definition.function.name.clone();
        self.inner.insert(name, entry);
        self
    }

    /// 批量注册工具
    pub fn register_all(mut self, entries: impl IntoIterator<Item = ToolEntry>) -> Self {
        for entry in entries {
            let name = entry.definition.function.name.clone();
            self.inner.insert(name, entry);
        }
        self
    }

    /// 构建不可变注册表
    pub fn build(self) -> ToolRegistry {
        ToolRegistry { inner: self.inner }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn dummy_handler() -> ToolFn {
        Arc::new(|_args, _ctx, _cancel| Box::pin(async { "ok".to_string() }))
    }

    fn make_entry(name: &str) -> ToolEntry {
        ToolEntry {
            definition: ToolDefinition::new(name, "测试工具"),
            handler: dummy_handler(),
            child_invisible: false,
        }
    }

    /// 构造标记为 child_invisible 的工具条目（递归防护测试用）
    fn make_child_invisible_entry(name: &str) -> ToolEntry {
        ToolEntry {
            definition: ToolDefinition::new(name, "对子 session 隐藏的工具"),
            handler: dummy_handler(),
            child_invisible: true,
        }
    }

    #[test]
    fn empty_registry() {
        let reg = ToolRegistry::builder().build();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.get("any").is_none());
        assert!(reg.definitions_json_for(false).is_empty());
        assert!(reg.definitions_json_for(true).is_empty());
    }

    #[test]
    fn register_and_get() {
        let reg = ToolRegistry::builder().register(make_entry("read")).build();
        assert!(!reg.is_empty());
        assert_eq!(reg.len(), 1);
        assert!(reg.get("read").is_some());
        assert!(reg.get("write").is_none());
    }

    #[test]
    fn register_all_batch() {
        let reg = ToolRegistry::builder()
            .register_all([make_entry("read"), make_entry("write"), make_entry("grep")])
            .build();
        assert_eq!(reg.len(), 3);
        assert!(reg.get("read").is_some());
        assert!(reg.get("write").is_some());
        assert!(reg.get("grep").is_some());
    }

    #[test]
    fn definitions_json_for_serializes_all_for_main_session() {
        let reg = ToolRegistry::builder()
            .register_all([make_entry("read"), make_entry("write")])
            .build();
        // 主 session（is_child=false）看到全部工具
        let defs = reg.definitions_json_for(false);
        assert_eq!(defs.len(), 2);
        // 每个都是 {type:"function", function:{name, description, parameters}}
        for d in &defs {
            assert_eq!(d["type"], "function");
            assert!(d["function"]["name"].is_string());
        }
    }

    #[test]
    fn definitions_json_for_hides_child_invisible_for_child_session() {
        // 注册 2 个普通工具 + 1 个对子 session 隐藏的工具
        let reg = ToolRegistry::builder()
            .register_all([
                make_entry("read"),
                make_entry("write"),
                make_child_invisible_entry("subagent"),
            ])
            .build();
        assert_eq!(reg.len(), 3);

        // 主 session（is_child=false）：看到全部 3 个
        let main_defs = reg.definitions_json_for(false);
        assert_eq!(main_defs.len(), 3);

        // 子 session（is_child=true）：只看到 2 个（subagent 被过滤）
        let child_defs = reg.definitions_json_for(true);
        assert_eq!(child_defs.len(), 2);
        let child_names: Vec<&str> = child_defs
            .iter()
            .map(|d| d["function"]["name"].as_str().unwrap_or(""))
            .collect();
        assert!(child_names.contains(&"read"));
        assert!(child_names.contains(&"write"));
        assert!(!child_names.contains(&"subagent"));
    }

    #[test]
    fn duplicate_name_overwrites() {
        let reg = ToolRegistry::builder()
            .register(make_entry("read"))
            .register(make_entry("read"))
            .build();
        assert_eq!(reg.len(), 1);
    }
}
