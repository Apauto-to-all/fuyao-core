//! 文件写入工具
//!
//! 提供文件写入功能，支持创建、覆盖文件，包含敏感路径保护、写入范围限制、
//! 覆写差异账本。
//!
//! 写入范围：仅允许工作目录与 fuyao_home 子树内的路径，清单外硬拒绝。

mod diff;
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
                "写入内容到文件，完全覆盖现有内容，文件不存在则创建（含父目录）；编辑用 edit 工具",
            )
            .string("path", "文件路径，不存在则创建，存在则覆盖")
            .required()
            .string("content", "写入内容")
            .required()
            .build(),
            tool_handler(write_file_impl),
            false,
        ),
    );
}
