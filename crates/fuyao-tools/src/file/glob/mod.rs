//! 文件名搜索工具
//!
//! 使用标准 glob 语法搜索文件名。
//! 基于 ignore crate（ripgrep 的目录遍历组件）实现，自动遵守 .gitignore 规则。
//!
//! ## 实现
//!
//! 使用 `ignore::WalkBuilder` 遍历目录树，`glob::Pattern` 匹配文件名。
//! 自动跳过隐藏文件和 .gitignore 排除的文件。
//! 搜索结果按修改时间排序（最新优先），支持 offset/limit 分页。

mod handler;
pub mod types;

use fuyao_api::ToolEntry;
use fuyao_api::{ToolDefinition, ToolFn};
use handler::glob_impl;
use serde_json::{Value, json};
use std::collections::HashMap;

/// 注册 glob 工具
pub fn register(map: &mut HashMap<&'static str, ToolEntry>) {
    let handler: ToolFn = std::sync::Arc::new(|args: Value, ctx, _cancel| {
        Box::pin(async move { glob_impl(args, &ctx).await })
    });

    let definition = ToolDefinition::builder(
        "glob",
        "使用标准 glob 语法搜索文件名。\n支持递归匹配（**/*.py）、通配符（*、?、[]）等标准 glob 模式。\n自动遵守 .gitignore 规则，跳过隐藏文件。",
    )
    .string("pattern", "标准 glob 模式")
    .required()
    .string("path", "搜索路径（默认当前目录）")
    .default(json!("."))
    .integer("limit", "最大结果数（默认 100）")
    .default(json!(100))
    .integer("offset", "跳过前 N 个结果（分页用）")
    .default(json!(0))
    .build();

    map.insert(
        "glob",
        ToolEntry {
            definition,
            handler,
            child_invisible: false,
        },
    );
}
