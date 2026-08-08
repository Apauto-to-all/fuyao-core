//! 文件读取工具
//!
//! 提供文件读取功能，支持分页、行号显示、设备文件保护、相似文件名建议、
//! 文件去重、外部编辑检测、循环检测、敏感信息脱敏。

mod handler;
pub mod types;

use fuyao_api::ToolEntry;
use fuyao_api::{ToolDefinition, ToolFn, ToolParameterProperty, ToolParameters};
use handler::read_file_impl;
use serde_json::Value;
use std::collections::HashMap;

/// 注册 read 工具
pub fn register(map: &mut HashMap<&'static str, ToolEntry>) {
    let handler: ToolFn = std::sync::Arc::new(|args: Value, ctx, _cancel| {
        Box::pin(async move { read_file_impl(args, &ctx) })
    });

    let mut properties = HashMap::new();
    properties.insert(
        "path".to_string(),
        ToolParameterProperty {
            kind: "string".to_string(),
            description: "文件或目录路径（支持绝对路径、相对路径、~/路径）".to_string(),
            default: None,
            enum_values: None,
            items: None,
        },
    );
    properties.insert(
        "offset".to_string(),
        ToolParameterProperty {
            kind: "integer".to_string(),
            description: "起始位置（从 1 开始，默认: 1）。文件：行号；目录：条目索引".to_string(),
            default: Some(serde_json::json!(1)),
            enum_values: None,
            items: None,
        },
    );
    properties.insert(
        "limit".to_string(),
        ToolParameterProperty {
            kind: "integer".to_string(),
            description: "最大读取数量（默认: 500，最大: 2000）。文件：行数；目录：条目数"
                .to_string(),
            default: Some(serde_json::json!(500)),
            enum_values: None,
            items: None,
        },
    );

    let definition = ToolDefinition {
        kind: "function".to_string(),
        function: fuyao_api::ToolSchema {
            name: "read".to_string(),
            description: "读取文件内容或列出目录内容。文件：返回带行号的内容，使用 offset/limit 分页。目录：返回目录下的文件和子目录列表，使用 offset/limit 分页。无法读取图片或二进制文件。文件不存在时会建议相似的文件名。自动检测重复读取和外部编辑。".to_string(),
            parameters: ToolParameters {
                kind: "object".to_string(),
                properties,
                required: vec!["path".to_string()],
            },
        },
    };

    map.insert(
        "read",
        ToolEntry {
            definition,
            handler,
            child_invisible: false,
        },
    );
}
