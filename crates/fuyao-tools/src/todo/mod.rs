//! Todo 任务管理工具模块
//!
//! 提供任务列表管理能力，用于拆解复杂任务、跟踪进度。
//! - 不传 todos 参数 → 读取当前列表
//! - 传了 todos → 整体覆盖写入

mod handler;
mod types;

use fuyao_api::{ToolDefinition, ToolEntry, insert_tool, tool_handler};
use handler::todo_handler;
use serde_json::json;
use std::collections::HashMap;

/// 注册 todowrite 工具
pub fn register(map: &mut HashMap<String, ToolEntry>) {
    // 嵌套的 todo 项 schema（被顶层 todos 数组的 items 引用）：
    // id / content 必填，status 四态枚举。用 ..Default::default() 补齐未设字段
    let mut todo_item_properties = HashMap::new();
    todo_item_properties.insert(
        "id".to_string(),
        fuyao_api::ToolParameterProperty {
            kind: "string".to_string(),
            description: "唯一标识（必填）".to_string(),
            ..Default::default()
        },
    );
    todo_item_properties.insert(
        "content".to_string(),
        fuyao_api::ToolParameterProperty {
            kind: "string".to_string(),
            description: "任务描述（必填）".to_string(),
            ..Default::default()
        },
    );
    todo_item_properties.insert(
        "status".to_string(),
        fuyao_api::ToolParameterProperty {
            kind: "string".to_string(),
            description: "状态，默认 pending".to_string(),
            enum_values: Some(vec![
                "pending".to_string(),
                "in_progress".to_string(),
                "completed".to_string(),
                "cancelled".to_string(),
            ]),
            ..Default::default()
        },
    );

    insert_tool(
        map,
        ToolEntry::new(
            ToolDefinition::builder(
                "todowrite",
                "管理任务列表。不传 todos 读取当前列表，传了则整体覆盖写入。\n字段：id（必填）、content（必填）、status(pending|in_progress|completed|cancelled)\n规则：顺序=优先级，同时只有一个 in_progress，完成即 completed",
            )
            .array("todos", "任务数组。不传则读取。")
            .items(HashMap::from([
                ("type".to_string(), json!("object")),
                ("properties".to_string(), json!(todo_item_properties)),
                ("required".to_string(), json!(["id", "content"])),
            ]))
            .build(),
            tool_handler(todo_handler),
            false,
        ),
    );
}
