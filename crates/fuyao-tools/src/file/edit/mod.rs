//! 文件补丁工具
//!
//! 提供文件编辑功能，支持模糊匹配替换和 V4A 格式补丁，包含外部编辑检测。
//!
//! ## 模式
//!
//! - **replace 模式**: 查找并替换文本，使用 9 种模糊匹配策略处理空白差异
//! - **patch 模式**: 应用 V4A 格式多文件补丁（支持 Add/Update/Delete/Move 操作）
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
pub mod patch;
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
        "文件补丁工具，支持两种模式：\nreplace 模式：查找并替换文本，使用模糊匹配处理空白差异。\npatch 模式：应用 V4A 格式多文件补丁。\n自动检测外部编辑并发出警告。",
    )
    .string("mode", "模式：replace（查找替换）或 patch（V4A 补丁）")
    .enum_values(["replace", "patch"])
    .default(json!("replace"))
    .required()
    .string("path", "文件路径（replace 模式必填）")
    .string("old_string", "要查找的文本（replace 模式必填）")
    .string("new_string", "替换文本（replace 模式必填）")
    .boolean("replace_all", "替换所有匹配（默认 false）")
    .default(json!(false))
    .string("patch", "V4A 格式补丁内容（patch 模式必填）")
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
