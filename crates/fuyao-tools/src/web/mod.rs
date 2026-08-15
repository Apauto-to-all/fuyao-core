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

use fuyao_api::{ToolDefinition, ToolEntry, insert_tool, tool_handler};
use handler::webfetch_handler;
use serde_json::json;
use std::collections::HashMap;

/// 注册 webfetch 工具
pub fn register(map: &mut HashMap<String, ToolEntry>) {
    // 超时/限制默认值从全局配置读取，反映到工具参数描述
    let limits = fuyao_api::get_config().tools.limits.clone();
    let webfetch_default_timeout = limits.webfetch_default_timeout_secs;
    let webfetch_max_timeout = limits.webfetch_max_timeout_secs;
    let webfetch_max_output = limits.webfetch_max_output_chars;

    insert_tool(
        map,
        ToolEntry::new(
            ToolDefinition::builder(
                "webfetch",
                "抓取 URL 内容并转为 markdown。仅支持公开可访问的 http/https 地址，私有网络地址会被阻止。",
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
            .build(),
            tool_handler(webfetch_handler),
            false,
        ),
    );
}
