//! 工具条目：schema 定义 + 执行 handler + 可见性元数据
//!
//! 作为工具系统各参与方（fuyao-tools 的静态注册表、fuyao-core 的引擎注册表、
//! fuyao-app 的装配）共享的唯一类型，消除此前两份字段同型结构体间的逐字段转换。

use crate::tool::{ToolDefinition, ToolFn};

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
