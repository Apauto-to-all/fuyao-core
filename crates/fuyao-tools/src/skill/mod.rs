//! Skills 工具模块
//!
//! 提供 Skill 加载和浏览能力。
//! - 不传参数 → 列出所有 Skills
//! - 传 name → 加载 Skill 内容
//! - 传 name + file_path → 加载关联文件

mod handler;
mod types;

use fuyao_api::ToolEntry;
use fuyao_api::{ToolDefinition, ToolFn};
use handler::skill_handler;
use std::collections::HashMap;

/// 注册 skill 工具
pub fn register(map: &mut HashMap<&'static str, ToolEntry>) {
    let handler: ToolFn = std::sync::Arc::new(|args, ctx, _cancel| {
        Box::pin(async move { skill_handler(args, &ctx) })
    });

    let definition = ToolDefinition::builder(
        "skill",
        "加载 Skill。不传参数：列出所有可用 Skills。传 name：加载指定 Skill 的完整内容。传 name + file_path：加载 Skill 的关联文件。",
    )
    .string("name", "Skill 名称（可选，不传则列出所有）")
    .string("file_path", "关联文件路径，如 'references/api.md'（可选）")
    .build();

    map.insert(
        "skill",
        ToolEntry {
            definition,
            handler,
            child_invisible: false,
        },
    );
}
