//! 文件内容搜索工具
//!
//! 使用正则表达式搜索文件内容。
//! 基于 grep crate（ripgrep 的库形式）实现，无需外部 rg 进程。
//!
//! ## 实现
//!
//! 使用 `grep-regex` 构建正则匹配器，`grep-searcher` 逐行搜索，
//! `ignore::WalkBuilder` 遍历目录树（自动遵守 .gitignore）。
//! 支持 include 参数过滤文件类型（如 *.py、*.{ts,tsx}）。
//! 支持 context 参数显示匹配行的上下文。
//! 搜索结果自动脱敏 API Key 等敏感信息。

mod handler;
pub mod types;

use fuyao_api::ToolEntry;
use fuyao_api::{ToolDefinition, ToolFn};
use handler::grep_impl;
use serde_json::{Value, json};
use std::collections::HashMap;

/// 注册 grep 工具
pub fn register(map: &mut HashMap<&'static str, ToolEntry>) {
    let handler: ToolFn = std::sync::Arc::new(|args: Value, ctx, _cancel| {
        Box::pin(async move { grep_impl(args, &ctx).await })
    });

    let definition = ToolDefinition::builder(
        "grep",
        "使用正则表达式搜索文件内容。\n支持正则表达式语法（如 log.*Error、function\\s+\\w+）。\n可使用 include 参数过滤文件类型（如 *.py）。\n自动遵守 .gitignore 规则。",
    )
    .string("pattern", "正则表达式（如 log.*Error、def\\s+\\w+）")
    .required()
    .string("path", "搜索路径（默认当前目录）")
    .default(json!("."))
    .string("include", "文件过滤模式（如 *.py、*.{ts,tsx}）")
    .integer("limit", "最大结果数（默认 50）")
    .default(json!(50))
    .integer("offset", "跳过前 N 个结果（分页用）")
    .default(json!(0))
    .integer("context", "显示匹配行的上下文行数")
    .default(json!(0))
    .build();

    map.insert(
        "grep",
        ToolEntry {
            definition,
            handler,
            child_invisible: false,
        },
    );
}
