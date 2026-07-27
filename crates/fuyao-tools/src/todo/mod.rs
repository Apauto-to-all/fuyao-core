//! Todo 任务管理工具模块
//!
//! 提供任务列表管理能力，用于拆解复杂任务、跟踪进度。
//! - 不传 todos 参数 → 读取当前列表
//! - 传了 todos → 整体覆盖写入

mod handler;
pub mod store;
mod types;

use crate::registry::ToolEntry;
use fuyao_api::{ToolDefinition, ToolFn, ToolParameterProperty, ToolParameters};
use handler::todo_handler;
use std::collections::HashMap;

/// 注册 todowrite 工具
pub fn register(map: &mut HashMap<&'static str, ToolEntry>) {
    let handler: ToolFn = std::sync::Arc::new(|args, ctx, _cancel| {
        Box::pin(async move { todo_handler(args, &ctx).await })
    });

    let mut todo_item_properties = HashMap::new();
    todo_item_properties.insert(
        "id".to_string(),
        ToolParameterProperty {
            kind: "string".to_string(),
            description: "唯一标识（必填）".to_string(),
            default: None,
            enum_values: None,
            items: None,
        },
    );
    todo_item_properties.insert(
        "content".to_string(),
        ToolParameterProperty {
            kind: "string".to_string(),
            description: "任务描述（必填）".to_string(),
            default: None,
            enum_values: None,
            items: None,
        },
    );
    todo_item_properties.insert(
        "status".to_string(),
        ToolParameterProperty {
            kind: "string".to_string(),
            description: "状态，默认 pending".to_string(),
            default: None,
            enum_values: Some(vec![
                "pending".to_string(),
                "in_progress".to_string(),
                "completed".to_string(),
                "cancelled".to_string(),
            ]),
            items: None,
        },
    );

    let mut properties = HashMap::new();
    properties.insert(
        "todos".to_string(),
        ToolParameterProperty {
            kind: "array".to_string(),
            description: "任务数组。不传则读取。".to_string(),
            default: None,
            enum_values: None,
            items: Some(HashMap::from([
                ("type".to_string(), serde_json::json!("object")),
                (
                    "properties".to_string(),
                    serde_json::json!(todo_item_properties),
                ),
                ("required".to_string(), serde_json::json!(["id", "content"])),
            ])),
        },
    );

    let definition = ToolDefinition {
        kind: "function".to_string(),
        function: fuyao_api::ToolSchema {
            name: "todowrite".to_string(),
            description: "管理任务列表。不传 todos 读取当前列表，传了则整体覆盖写入。\n字段：id（必填）、content（必填）、status(pending|in_progress|completed|cancelled)\n规则：顺序=优先级，同时只有一个 in_progress，完成即 completed".to_string(),
            parameters: ToolParameters {
                kind: "object".to_string(),
                properties,
                required: vec![],
            },
        },
    };

    map.insert(
        "todowrite",
        ToolEntry {
            definition,
            handler,
            child_invisible: false,
        },
    );
}
