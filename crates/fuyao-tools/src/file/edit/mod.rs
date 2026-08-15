//! 文件编辑工具
//!
//! 提供查找替换式文件编辑，使用模糊匹配处理空白差异，包含外部编辑检测。
//!
//! ## 安全
//!
//! - 编辑前检查敏感路径（同 write 工具）
//! - 编辑前检测外部编辑并发出警告
//! - 编辑后更新追踪器时间戳

pub mod backend;
pub mod filelock;
pub mod fuzzy;
mod handler;
pub mod textutil;
pub mod types;

use fuyao_api::ToolEntry;
use fuyao_api::{ToolDefinition, ToolFn};
use handler::edit_impl;
use serde_json::{Value, json};
use std::collections::HashMap;

/// 注册 edit 工具
pub fn register(map: &mut HashMap<&'static str, ToolEntry>) {
    let handler: ToolFn = std::sync::Arc::new(|args: Value, ctx, _cancel| {
        Box::pin(async move { edit_impl(args, &ctx) })
    });

    let definition = ToolDefinition::builder(
        "edit",
        "文件编辑工具：查找并替换文本，使用模糊匹配处理空白差异，自动检测外部编辑并发出警告。",
    )
    .string("path", "文件路径")
    .required()
    .string("old_string", "要查找的文本")
    .required()
    .string("new_string", "替换文本")
    .required()
    .boolean("replace_all", "替换所有匹配（默认 false）")
    .default(json!(false))
    .build();

    map.insert(
        "edit",
        ToolEntry {
            definition,
            handler,
            child_invisible: false,
        },
    );
}
