//! WebFetch 工具模块
//!
//! 从 URL 抓取内容并转换为指定格式。
//! 支持 markdown（默认）、html（原始）两种输出格式。
//! 自动处理同域名重定向，跨域名重定向会提示新 URL。
//! 安全限制：阻止私有网络地址和云元数据端点。
//! 分页支持：使用 offset/limit 参数获取部分内容。

mod cache;
mod converter;
mod handler;
mod pagination;
mod redirect;
mod safety;
mod types;

use crate::registry::ToolEntry;
use fuyao_api::{ToolDefinition, ToolFn, ToolParameterProperty, ToolParameters};
use handler::webfetch_handler;
use std::collections::HashMap;

/// 注册 webfetch 工具
pub fn register(map: &mut HashMap<&'static str, ToolEntry>) {
    let handler: ToolFn = std::sync::Arc::new(|args, _ctx, _cancel| {
        Box::pin(async move { webfetch_handler(args).await })
    });

    // 超时/限制默认值从全局配置读取，反映到工具参数描述
    let limits = fuyao_api::get_config().tools.limits.clone();
    let webfetch_default_timeout = limits.webfetch_default_timeout_secs;
    let webfetch_max_timeout = limits.webfetch_max_timeout_secs;
    let webfetch_max_output = limits.webfetch_max_output_chars;

    let mut properties = HashMap::new();
    properties.insert(
        "url".to_string(),
        ToolParameterProperty {
            kind: "string".to_string(),
            description: "要抓取的 URL（必须以 http:// 或 https:// 开头）".to_string(),
            default: None,
            enum_values: None,
            items: None,
        },
    );
    properties.insert(
        "output_format".to_string(),
        ToolParameterProperty {
            kind: "string".to_string(),
            description: "输出格式：markdown（默认）、html".to_string(),
            default: Some(serde_json::json!("markdown")),
            enum_values: Some(vec!["markdown".to_string(), "html".to_string()]),
            items: None,
        },
    );
    properties.insert(
        "timeout".to_string(),
        ToolParameterProperty {
            kind: "integer".to_string(),
            description: format!(
                "超时时间（秒），默认 {webfetch_default_timeout}，最大 {webfetch_max_timeout}"
            ),
            default: Some(serde_json::json!(webfetch_default_timeout)),
            enum_values: None,
            items: None,
        },
    );
    properties.insert(
        "offset".to_string(),
        ToolParameterProperty {
            kind: "integer".to_string(),
            description: "跳过前面的字符数（默认 0）".to_string(),
            default: Some(serde_json::json!(0)),
            enum_values: None,
            items: None,
        },
    );
    properties.insert(
        "limit".to_string(),
        ToolParameterProperty {
            kind: "integer".to_string(),
            description: format!("限制返回的字符数（默认 {webfetch_max_output}）"),
            default: Some(serde_json::json!(webfetch_max_output)),
            enum_values: None,
            items: None,
        },
    );

    let definition = ToolDefinition {
        kind: "function".to_string(),
        function: fuyao_api::ToolSchema {
            name: "webfetch".to_string(),
            description: "从 URL 抓取内容并转换为指定格式。\n支持 markdown（默认）、html（原始）两种输出格式。\n自动处理同域名重定向，跨域名重定向会提示新 URL。\n安全限制：阻止私有网络地址和云元数据端点。\n分页支持：使用 offset/limit 参数获取部分内容。".to_string(),
            parameters: ToolParameters {
                kind: "object".to_string(),
                properties,
                required: vec!["url".to_string()],
            },
        },
    };

    map.insert(
        "webfetch",
        ToolEntry {
            definition,
            handler,
            child_invisible: false,
        },
    );
}
