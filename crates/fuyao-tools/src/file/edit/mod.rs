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

use fuyao_api::{ToolDefinition, ToolEntry, insert_tool, tool_handler};
use handler::edit_impl;
use serde_json::json;
use std::collections::HashMap;

/// 注册 edit 工具
pub fn register(map: &mut HashMap<String, ToolEntry>) {
    insert_tool(
        map,
        ToolEntry::new(
            ToolDefinition::builder(
                "edit",
                "查找替换式文件编辑。old_string 必须与文件内容精确匹配，并包含足够上下文以唯一定位；多处匹配时指定 replace_all 或补充更多上下文。编辑前先 read 确认现有内容，不要凭记忆修改。",
            )
            .string("path", "文件路径")
            .required()
            .string("old_string", "要查找的文本")
            .required()
            .string("new_string", "替换文本")
            .required()
            .boolean("replace_all", "替换所有匹配（默认 false）")
            .default(json!(false))
            .build(),
            tool_handler(edit_impl),
            false,
        ),
    );
}
