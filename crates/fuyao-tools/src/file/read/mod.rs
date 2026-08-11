//! 文件读取工具
//!
//! 提供文件读取功能，支持分页、行号显示、设备文件保护、相似文件名建议、
//! 文件去重、外部编辑检测、循环检测、敏感信息脱敏。

mod handler;
pub mod types;

use fuyao_api::ToolEntry;
use fuyao_api::{ToolDefinition, ToolFn};
use handler::read_file_impl;
use serde_json::{Value, json};
use std::collections::HashMap;

/// 注册 read 工具
pub fn register(map: &mut HashMap<&'static str, ToolEntry>) {
    let handler: ToolFn = std::sync::Arc::new(|args: Value, ctx, _cancel| {
        Box::pin(async move { read_file_impl(args, &ctx) })
    });

    let definition = ToolDefinition::builder(
        "read",
        "读取文件内容或列出目录内容。文件：返回带行号的内容，使用 offset/limit 分页。目录：返回目录下的文件和子目录列表，使用 offset/limit 分页。无法读取图片或二进制文件。文件不存在时会建议相似的文件名。自动检测重复读取和外部编辑。",
    )
    .string("path", "文件或目录路径（支持绝对路径、相对路径、~/路径）")
    .required()
    .integer("offset", "起始位置（从 1 开始，默认: 1）。文件：行号；目录：条目索引")
    .default(json!(1))
    .integer("limit", "最大读取数量（默认: 500，最大: 2000）。文件：行数；目录：条目数")
    .default(json!(500))
    .build();

    map.insert(
        "read",
        ToolEntry {
            definition,
            handler,
            child_invisible: false,
        },
    );
}
