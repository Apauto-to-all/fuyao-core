//! 文件写入工具
//!
//! 提供文件写入功能，支持创建、覆盖文件，包含敏感路径保护、外部编辑检测。

mod handler;
pub mod types;

use fuyao_api::{ToolDefinition, ToolEntry, insert_tool, tool_handler};
use handler::write_file_impl;
use std::collections::HashMap;

/// 注册 write 工具
pub fn register(map: &mut HashMap<String, ToolEntry>) {
    insert_tool(
        map,
        ToolEntry::new(
            ToolDefinition::builder(
                "write",
                "写入内容到文件，完全覆盖现有内容。如果文件不存在则创建，父目录不存在则自动创建。如需部分编辑，请使用 edit 工具。注意：敏感路径（如 SSH 密钥、系统配置）将被拒绝写入。自动检测外部编辑并发出警告。",
            )
            .string("path", "文件路径（不存在则创建，存在则覆盖）")
            .required()
            .string("content", "要写入的完整内容")
            .required()
            .build(),
            tool_handler(write_file_impl),
            false,
        ),
    );
}
