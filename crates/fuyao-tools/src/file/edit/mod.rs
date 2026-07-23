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

use crate::registry::ToolEntry;
use fuyao_api::{ToolDefinition, ToolFn, ToolParameterProperty, ToolParameters};
use handler::edit_impl;
use serde_json::Value;
use std::collections::HashMap;

/// 注册 edit 工具
pub fn register(map: &mut HashMap<&'static str, ToolEntry>) {
    let handler: ToolFn =
        std::sync::Arc::new(|args: Value, ctx| Box::pin(async move { edit_impl(args, &ctx) }));

    let mut properties = HashMap::new();
    properties.insert(
        "mode".to_string(),
        ToolParameterProperty {
            kind: "string".to_string(),
            description: "模式：replace（查找替换）或 patch（V4A 补丁）".to_string(),
            default: Some(serde_json::json!("replace")),
            enum_values: Some(vec!["replace".to_string(), "patch".to_string()]),
            items: None,
        },
    );
    properties.insert(
        "path".to_string(),
        ToolParameterProperty {
            kind: "string".to_string(),
            description: "文件路径（replace 模式必填）".to_string(),
            default: None,
            enum_values: None,
            items: None,
        },
    );
    properties.insert(
        "old_string".to_string(),
        ToolParameterProperty {
            kind: "string".to_string(),
            description: "要查找的文本（replace 模式必填）".to_string(),
            default: None,
            enum_values: None,
            items: None,
        },
    );
    properties.insert(
        "new_string".to_string(),
        ToolParameterProperty {
            kind: "string".to_string(),
            description: "替换文本（replace 模式必填）".to_string(),
            default: None,
            enum_values: None,
            items: None,
        },
    );
    properties.insert(
        "replace_all".to_string(),
        ToolParameterProperty {
            kind: "boolean".to_string(),
            description: "替换所有匹配（默认 false）".to_string(),
            default: Some(serde_json::json!(false)),
            enum_values: None,
            items: None,
        },
    );
    properties.insert(
        "patch".to_string(),
        ToolParameterProperty {
            kind: "string".to_string(),
            description: "V4A 格式补丁内容（patch 模式必填）".to_string(),
            default: None,
            enum_values: None,
            items: None,
        },
    );

    let definition = ToolDefinition {
        kind: "function".to_string(),
        function: fuyao_api::ToolSchema {
            name: "edit".to_string(),
            description: "文件补丁工具，支持两种模式：\nreplace 模式：查找并替换文本，使用模糊匹配处理空白差异。\npatch 模式：应用 V4A 格式多文件补丁。\n自动检测外部编辑并发出警告。".to_string(),
            parameters: ToolParameters {
                kind: "object".to_string(),
                properties,
                required: vec!["mode".to_string()],
            },
        },
    };

    map.insert(
        "edit",
        ToolEntry {
            definition,
            handler,
        },
    );
}
