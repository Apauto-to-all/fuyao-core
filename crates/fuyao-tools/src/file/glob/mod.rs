//! 文件名搜索工具
//!
//! 使用标准 glob 语法搜索文件名。
//! 基于 ignore crate（ripgrep 的目录遍历组件）实现，自动遵守 .gitignore 规则。
//!
//! ## 实现
//!
//! 使用 `ignore::WalkBuilder` 遍历目录树，`glob::Pattern` 匹配文件名。
//! 自动跳过隐藏文件和 .gitignore 排除的文件。
//! 搜索结果按修改时间排序（最新优先），结果数受配置硬上限约束，超出自动截断。

mod handler;
pub mod types;

use fuyao_api::{ToolDefinition, ToolEntry, insert_tool, tool_handler};
use handler::glob_impl;
use serde_json::json;
use std::collections::HashMap;
use types::{DEFAULT_LIMIT, DEFAULT_PATH};

/// 注册 glob 工具
pub fn register(map: &mut HashMap<String, ToolEntry>) {
    insert_tool(
        map,
        ToolEntry::new(
            ToolDefinition::builder(
                "glob",
                "使用标准 glob 语法搜索文件名。\n支持递归匹配（**/*.py）、通配符（*、?、[]）等标准 glob 模式。\n自动遵守 .gitignore 规则，跳过隐藏文件。",
            )
            .string("pattern", "标准 glob 模式")
            .required()
            .string("path", format!("搜索路径（默认 {DEFAULT_PATH}）"))
            .default(json!(DEFAULT_PATH))
            .integer("limit", format!("最大结果数（默认 {DEFAULT_LIMIT}，有硬上限，超出自动截断）"))
            .default(json!(DEFAULT_LIMIT))
            .build(),
            tool_handler(glob_impl),
            false,
        ),
    );
}
