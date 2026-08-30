//! 文件名搜索工具
//!
//! 按 gitignore 语义的 glob 模式搜索文件名（支持 `!` 前缀排除与 `{a,b}` 花括号展开）。
//! 基于 ignore crate（ripgrep 的目录遍历组件）实现，自动遵守 .gitignore 规则。
//!
//! ## 实现
//!
//! 使用 `ignore::WalkBuilder` 遍历目录树，`ignore::overrides::OverrideBuilder`
//! 编译模式并对遍历结果逐项判定。
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
                "按 glob 模式递归搜索文件名，遵守 .gitignore 规则。比终端 find 更安全、结构化、跨平台一致，搜索文件名优先使用本工具",
            )
                .string("pattern", "glob 模式（如 *.rs、src/*.rs、!*.log、*.{md,txt}）")
                .required()
                .string("path", format!("搜索路径（默认 {DEFAULT_PATH}）"))
                .default(json!(DEFAULT_PATH))
                .integer(
                    "limit",
                    format!("最大结果数（默认 {DEFAULT_LIMIT}）"),
                )
                .default(json!(DEFAULT_LIMIT))
                .build(),
            tool_handler(glob_impl),
            false,
        ),
    );
}
