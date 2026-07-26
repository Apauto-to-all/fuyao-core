//! 文件名搜索工具
//!
//! 使用标准 glob 语法搜索文件名。
//! 基于 ignore crate（ripgrep 的目录遍历组件）实现，自动遵守 .gitignore 规则。
//!
//! ## 实现
//!
//! 使用 `ignore::WalkBuilder` 遍历目录树，`glob::Pattern` 匹配文件名。
//! 自动跳过隐藏文件和 .gitignore 排除的文件。
//! 搜索结果按修改时间排序（最新优先），支持 offset/limit 分页。

mod handler;
pub mod types;

use crate::registry::ToolEntry;
use fuyao_api::{ToolDefinition, ToolFn, ToolParameterProperty, ToolParameters};
use handler::glob_impl;
use serde_json::Value;
use std::collections::HashMap;

/// 注册 glob 工具
pub fn register(map: &mut HashMap<&'static str, ToolEntry>) {
    let handler: ToolFn = std::sync::Arc::new(|args: Value, ctx, _cancel| {
        Box::pin(async move { glob_impl(args, &ctx).await })
    });

    let mut properties = HashMap::new();
    properties.insert(
        "pattern".to_string(),
        ToolParameterProperty {
            kind: "string".to_string(),
            description: "标准 glob 模式".to_string(),
            default: None,
            enum_values: None,
            items: None,
        },
    );
    properties.insert(
        "path".to_string(),
        ToolParameterProperty {
            kind: "string".to_string(),
            description: "搜索路径（默认当前目录）".to_string(),
            default: Some(serde_json::json!(".")),
            enum_values: None,
            items: None,
        },
    );
    properties.insert(
        "limit".to_string(),
        ToolParameterProperty {
            kind: "integer".to_string(),
            description: "最大结果数（默认 100）".to_string(),
            default: Some(serde_json::json!(100)),
            enum_values: None,
            items: None,
        },
    );
    properties.insert(
        "offset".to_string(),
        ToolParameterProperty {
            kind: "integer".to_string(),
            description: "跳过前 N 个结果（分页用）".to_string(),
            default: Some(serde_json::json!(0)),
            enum_values: None,
            items: None,
        },
    );

    let definition = ToolDefinition {
        kind: "function".to_string(),
        function: fuyao_api::ToolSchema {
            name: "glob".to_string(),
            description: "使用标准 glob 语法搜索文件名。\n支持递归匹配（**/*.py）、通配符（*、?、[]）等标准 glob 模式。\n自动遵守 .gitignore 规则，跳过隐藏文件。".to_string(),
            parameters: ToolParameters {
                kind: "object".to_string(),
                properties,
                required: vec!["pattern".to_string()],
            },
        },
    };

    map.insert(
        "glob",
        ToolEntry {
            definition,
            handler,
        },
    );
}
