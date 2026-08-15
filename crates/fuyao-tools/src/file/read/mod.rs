//! 文件读取工具
//!
//! 提供文件读取功能，支持分页、行号显示、设备文件保护、相似文件名建议、
//! 外部编辑检测、循环检测、敏感信息脱敏。

mod handler;
pub mod types;

use fuyao_api::{ToolDefinition, ToolEntry, insert_tool, tool_handler};
use handler::read_file_impl;
use serde_json::json;
use std::collections::HashMap;
use types::{DEFAULT_LIMIT, DEFAULT_OFFSET};

/// 注册 read 工具
pub fn register(map: &mut HashMap<String, ToolEntry>) {
    insert_tool(
        map,
        ToolEntry::new(
            ToolDefinition::builder(
                "read",
                "读取文件内容或列出目录内容。文件：返回带行号的内容，使用 offset/limit 分页。目录：返回目录下的文件和子目录列表，使用 offset/limit 分页。无法读取图片或二进制文件。文件不存在时会建议相似的文件名。自动检测外部编辑。",
            )
            .string("path", "文件或目录路径（支持绝对路径、相对路径、~/路径）")
            .required()
            .integer("offset", "起始位置（从 1 开始，默认: 1）。文件：行号；目录：条目索引")
            .default(json!(DEFAULT_OFFSET))
            .integer(
                "limit",
                format!("最大读取数量（默认: {DEFAULT_LIMIT}，最大: {}）。文件：行数；目录：条目数", types::MAX_LIMIT),
            )
            .default(json!(DEFAULT_LIMIT))
            .build(),
            tool_handler(read_file_impl),
            false,
        ),
    );
}
