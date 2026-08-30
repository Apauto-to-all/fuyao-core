//! 文件内容搜索工具
//!
//! 使用正则表达式搜索文件内容。
//! 基于 grep crate（ripgrep 的库形式）实现，无需外部 rg 进程。
//!
//! ## 实现
//!
//! 使用 `grep-regex` 构建正则匹配器，`grep-searcher` 逐行搜索，
//! `ignore::WalkBuilder` 遍历目录树（自动遵守 .gitignore）。
//! 支持 glob 参数以 glob 语法过滤文件（如 *.py、*.{ts,tsx}，`!` 前缀排除）。
//! 搜索结果自动脱敏 API Key 等敏感信息。

mod handler;
pub mod types;

use fuyao_api::{ToolDefinition, ToolEntry, insert_tool, tool_handler};
use handler::grep_impl;
use serde_json::json;
use std::collections::HashMap;
use types::{DEFAULT_LIMIT, DEFAULT_PATH};

/// 注册 grep 工具
pub fn register(map: &mut HashMap<String, ToolEntry>) {
    insert_tool(
        map,
        ToolEntry::new(
            ToolDefinition::builder(
                "grep",
                "按正则表达式搜索文件内容（大小写不敏感），遵守 .gitignore 规则。比终端 grep 更安全、结构化、跨平台一致，搜索内容优先使用本工具",
            )
            .string("pattern", "正则表达式（大小写不敏感）")
            .required()
            .string("path", format!("搜索路径（默认 {DEFAULT_PATH}）"))
            .default(json!(DEFAULT_PATH))
            .string("glob", "glob 过滤模式（如 *.rs、*.{ts,tsx}，`!` 前缀排除）")
            .integer(
                "limit",
                format!("最大结果数（默认 {DEFAULT_LIMIT}，有硬上限，超出自动截断）"),
            )
            .default(json!(DEFAULT_LIMIT))
            .build(),
            tool_handler(grep_impl),
            false,
        ),
    );
}
