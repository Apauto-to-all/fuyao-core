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

use fuyao_api::ToolEntry;
use fuyao_api::{ToolDefinition, ToolFn};
use handler::webfetch_handler;
use serde_json::json;
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

    let definition = ToolDefinition::builder(
        "webfetch",
        "从 URL 抓取内容并转换为指定格式。\n支持 markdown（默认）、html（原始）两种输出格式。\n自动处理同域名重定向，跨域名重定向会提示新 URL。\n安全限制：阻止私有网络地址和云元数据端点。\n分页支持：使用 offset/limit 参数获取部分内容。",
    )
    .string("url", "要抓取的 URL（必须以 http:// 或 https:// 开头）")
    .required()
    .string("output_format", "输出格式：markdown（默认）、html")
    .default(json!("markdown"))
    .enum_values(["markdown", "html"])
    .integer("timeout", format!("超时时间（秒），默认 {webfetch_default_timeout}，最大 {webfetch_max_timeout}"))
    .default(json!(webfetch_default_timeout))
    .integer("offset", "跳过前面的字符数（默认 0）")
    .default(json!(0))
    .integer("limit", format!("限制返回的字符数（默认 {webfetch_max_output}）"))
    .default(json!(webfetch_max_output))
    .build();

    map.insert(
        "webfetch",
        ToolEntry {
            definition,
            handler,
            child_invisible: false,
        },
    );
}
