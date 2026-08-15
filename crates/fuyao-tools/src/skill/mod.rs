//! Skills 工具模块
//!
//! 提供 Skill 加载和浏览能力。
//! - 不传参数 → 列出所有 Skills
//! - 传 name → 加载 Skill 内容
//! - 传 name + file_path → 加载关联文件

mod handler;
mod types;

use fuyao_api::{ToolDefinition, ToolEntry, insert_tool, tool_handler};
use handler::skill_handler;
use std::collections::HashMap;

/// 注册 skill 工具
pub fn register(map: &mut HashMap<String, ToolEntry>) {
    insert_tool(
        map,
        ToolEntry::new(
            ToolDefinition::builder(
                "skill",
                "Skill 加载：不传参数列出全部；传 name 加载内容；再传 file_path 加载其关联文件。",
            )
            .string("name", "Skill 名称")
            .string("file_path", "关联文件路径，如 'references/api.md'（可选）")
            .build(),
            tool_handler(skill_handler),
            false,
        ),
    );
}
