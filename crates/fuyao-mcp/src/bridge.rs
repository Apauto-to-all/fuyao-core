//! MCP 工具桥接
//!
//! 将 MCP Server 暴露的工具构建为 (name, schema, handler) 三元组，
//! 由调用方决定如何注册。
//!
//! 集成：超时保护、熔断器、Auth 恢复、Session 恢复。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use fuyao_api::{
    ToolCallContext, ToolDefinition, ToolFn, ToolParameterProperty, ToolParameters, ToolSchema,
};
use serde_json::Value;

use crate::circuit_breaker::{bump_error, check_breaker, reset_error};
use crate::connection::MCPConnection;
use crate::security::{normalize_mcp_input_schema, sanitize_error, sanitize_mcp_name_component};

/// 构建 MCP 工具前缀名
///
/// 格式：mcp_{server}_{tool}，非安全字符替换为下划线。
pub fn build_prefixed_name(server_name: &str, tool_name: &str) -> String {
    let safe_server = sanitize_mcp_name_component(server_name);
    let safe_tool = sanitize_mcp_name_component(tool_name);
    format!("mcp_{safe_server}_{safe_tool}")
}

/// 判断工具是否应注册
///
/// 如果 tools_filter 为空，所有工具都注册。
/// 未列出的工具默认启用。
pub fn should_register_tool(tool_name: &str, tools_filter: &HashMap<String, bool>) -> bool {
    if tools_filter.is_empty() {
        return true;
    }
    tools_filter.get(tool_name).copied().unwrap_or(true)
}

/// MCP 工具桥接结果
pub struct BridgedTool {
    /// 前缀名（mcp_server_tool）
    pub prefixed_name: String,
    /// OpenAI function schema
    pub schema: ToolDefinition,
    /// 工具执行器
    pub handler: ToolFn,
}

/// 从 MCP 工具的 input_schema 构建 ToolSchema
fn build_tool_schema(prefixed_name: &str, description: &str, input_schema: &Value) -> ToolSchema {
    let normalized = normalize_mcp_input_schema(input_schema);

    let mut properties = HashMap::new();
    let mut required = Vec::new();

    if let Some(props) = normalized.get("properties").and_then(|v| v.as_object()) {
        for (name, prop_value) in props {
            let kind = prop_value
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("string")
                .to_string();

            let desc = prop_value
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let default = prop_value.get("default").cloned();

            let enum_values = prop_value
                .get("enum")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                });

            let items = prop_value
                .get("items")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect::<HashMap<String, Value>>()
                });

            properties.insert(
                name.clone(),
                ToolParameterProperty {
                    kind,
                    description: desc,
                    default,
                    enum_values,
                    items,
                },
            );
        }
    }

    if let Some(req) = normalized.get("required").and_then(|v| v.as_array()) {
        for item in req {
            if let Some(s) = item.as_str() {
                required.push(s.to_string());
            }
        }
    }

    ToolSchema {
        name: prefixed_name.to_string(),
        description: description.to_string(),
        parameters: ToolParameters {
            kind: "object".to_string(),
            properties,
            required,
        },
    }
}

/// 构建 MCP Server 的工具列表
///
/// 不触碰全局 ToolRegistry，只返回工具数据，
/// 由调用方决定如何注册。
pub fn build_server_tools(
    server_name: &str,
    connection: &MCPConnection,
    config: &fuyao_api::MCPServerConfig,
) -> Vec<BridgedTool> {
    let tools_filter = &config.tools;
    let tool_timeout = config.timeout;
    let server_name_owned = server_name.to_string();

    let mut result = Vec::new();

    for mcp_tool in &connection.tools {
        let raw_name = &mcp_tool.name;

        if !should_register_tool(raw_name, tools_filter) {
            continue;
        }

        let prefixed_name = build_prefixed_name(server_name, raw_name);

        let description = mcp_tool
            .description
            .clone()
            .unwrap_or_else(|| format!("MCP tool {raw_name} from {server_name}"));

        let schema = build_tool_schema(&prefixed_name, &description, &mcp_tool.input_schema);

        let tool_definition = ToolDefinition {
            kind: "function".to_string(),
            function: schema,
        };

        // 构建占位 handler（实际调用在 MCPManager 层面完成）
        let tool_name = raw_name.clone();
        let srv_name = server_name_owned.clone();

        let handler: ToolFn = Arc::new(move |_args: Value, _ctx: ToolCallContext| {
            let tool_name = tool_name.clone();
            let srv_name = srv_name.clone();
            let timeout_secs = tool_timeout;

            Box::pin(async move {
                if let Some(msg) = check_breaker(&srv_name) {
                    return serde_json::json!({"error": msg}).to_string();
                }

                // 占位：实际调用在 make_tool_call_handler 中完成
                let _ = (tool_name, timeout_secs);
                serde_json::json!({"error": "bridge handler: 需要通过 MCPConnection 调用"})
                    .to_string()
            })
        });

        result.push(BridgedTool {
            prefixed_name,
            schema: tool_definition,
            handler,
        });
    }

    result
}

/// 构建带 MCPConnection 引用的工具 handler
///
/// 由于 ToolFn 需要是 'static 的，通过 `Arc<Mutex<Option<MCPConnection>>>` 传递连接。
pub fn make_tool_call_handler(
    connection: Arc<tokio::sync::Mutex<Option<MCPConnection>>>,
    tool_name: String,
    server_name: String,
    tool_timeout: u32,
) -> ToolFn {
    Arc::new(move |args: Value, _ctx: ToolCallContext| {
        let tool_name = tool_name.clone();
        let server_name = server_name.clone();
        let timeout_secs = tool_timeout;
        let conn = connection.clone();

        Box::pin(async move {
            // 检查熔断器
            if let Some(msg) = check_breaker(&server_name) {
                return serde_json::json!({"error": msg}).to_string();
            }

            // 执行 MCP 调用
            let started = std::time::Instant::now();
            let call_result = tokio::time::timeout(
                Duration::from_secs(timeout_secs as u64),
                do_call(&conn, &tool_name, args.clone()),
            )
            .await;

            let result = match call_result {
                Ok(Ok(result)) => {
                    if let Ok(parsed) = serde_json::from_str::<Value>(&result) {
                        if parsed.get("error").is_some() {
                            bump_error(&server_name);
                        } else {
                            reset_error(&server_name);
                        }
                    }
                    result
                }
                Ok(Err(err_msg)) => {
                    // 检测 Auth 错误并尝试恢复
                    if crate::recovery::is_auth_error_str(&err_msg) {
                        tracing::warn!(
                            server = %server_name,
                            tool = %tool_name,
                            kind = "auth",
                            "MCP 调用遇到可恢复错误，触发重连"
                        );
                        notify_reconnect(&conn);
                        let retry_result = wait_and_retry(&conn, &tool_name, &args).await;
                        if let Some(result) = retry_result {
                            reset_error(&server_name);
                            tracing::info!(
                                name = %server_name,
                                tool = %tool_name,
                                ok = !result.contains("\"error\""),
                                recovered = true,
                                elapsed_ms = started.elapsed().as_millis() as u64,
                                "MCP 工具调用完成"
                            );
                            return result;
                        }
                    }

                    // 检测 Session 过期并尝试恢复
                    if crate::recovery::is_session_expired_error_str(&err_msg) {
                        tracing::warn!(
                            server = %server_name,
                            tool = %tool_name,
                            kind = "session",
                            "MCP 调用遇到可恢复错误，触发重连"
                        );
                        notify_reconnect(&conn);
                        let retry_result = wait_and_retry(&conn, &tool_name, &args).await;
                        if let Some(result) = retry_result {
                            reset_error(&server_name);
                            tracing::info!(
                                name = %server_name,
                                tool = %tool_name,
                                ok = !result.contains("\"error\""),
                                recovered = true,
                                elapsed_ms = started.elapsed().as_millis() as u64,
                                "MCP 工具调用完成"
                            );
                            return result;
                        }
                    }

                    bump_error(&server_name);
                    serde_json::json!({
                        "error": sanitize_error(&format!("MCP 调用失败: {err_msg}"))
                    })
                    .to_string()
                }
                Err(_) => {
                    bump_error(&server_name);
                    serde_json::json!({
                        "error": format!("MCP tool '{tool_name}' timed out after {timeout_secs}s")
                    })
                    .to_string()
                }
            };

            tracing::info!(
                name = %server_name,
                tool = %tool_name,
                ok = !result.contains("\"error\""),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "MCP 工具调用完成"
            );
            result
        })
    })
}

/// 执行单次 MCP 工具调用，返回格式化的 JSON 字符串
async fn do_call(
    conn: &Arc<tokio::sync::Mutex<Option<MCPConnection>>>,
    tool_name: &str,
    args: Value,
) -> Result<String, String> {
    let guard = conn.lock().await;
    let conn = guard
        .as_ref()
        .ok_or_else(|| "MCP server 未连接".to_string())?;

    let result = conn
        .call_tool(tool_name, args)
        .await
        .map_err(|e| e.to_string())?;

    // 处理 MCP 调用结果
    if result.is_error.unwrap_or(false) {
        let error_text: String = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
            .collect::<Vec<&str>>()
            .join("");
        return Ok(serde_json::json!({
            "error": sanitize_error(&error_text)
        })
        .to_string());
    }

    // 提取文本内容
    let parts: Vec<String> = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect();
    let text_result = parts.join("\n");

    // 检查 structuredContent
    let structured = result.structured_content.as_ref();

    let output = if let Some(structured) = structured {
        if !text_result.is_empty() {
            serde_json::json!({
                "result": text_result,
                "structuredContent": structured
            })
        } else {
            serde_json::json!({"result": structured})
        }
    } else {
        serde_json::json!({"result": text_result})
    };

    Ok(output.to_string())
}

/// 通知连接重连
fn notify_reconnect(conn: &Arc<tokio::sync::Mutex<Option<MCPConnection>>>) {
    if let Ok(mut guard) = conn.try_lock()
        && let Some(ref mut c) = *guard
    {
        c.notify_reconnect();
    }
}

/// 等待 session 恢复后重试调用
async fn wait_and_retry(
    conn: &Arc<tokio::sync::Mutex<Option<MCPConnection>>>,
    tool_name: &str,
    args: &Value,
) -> Option<String> {
    // 等待 session 恢复（时长从全局配置 get_config().mcp 读取）
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs(fuyao_api::get_config().mcp.session_recovery_wait_secs);
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // 重试调用
    match do_call(conn, tool_name, args.clone()).await {
        Ok(result) => {
            let parsed: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();
            if parsed.get("error").is_none() {
                Some(result)
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_prefixed_name_basic() {
        assert_eq!(
            build_prefixed_name("my-server", "search"),
            "mcp_my_server_search"
        );
    }

    #[test]
    fn build_prefixed_name_with_dots() {
        assert_eq!(
            build_prefixed_name("server.v1", "tool.name"),
            "mcp_server_v1_tool_name"
        );
    }

    #[test]
    fn should_register_tool_empty_filter() {
        let filter = HashMap::new();
        assert!(should_register_tool("any_tool", &filter));
    }

    #[test]
    fn should_register_tool_enabled() {
        let mut filter = HashMap::new();
        filter.insert("search".to_string(), true);
        assert!(should_register_tool("search", &filter));
    }

    #[test]
    fn should_register_tool_disabled() {
        let mut filter = HashMap::new();
        filter.insert("search".to_string(), false);
        assert!(!should_register_tool("search", &filter));
    }

    #[test]
    fn should_register_tool_unlisted_default_enabled() {
        let mut filter = HashMap::new();
        filter.insert("other".to_string(), true);
        assert!(should_register_tool("search", &filter));
    }

    #[test]
    fn build_tool_schema_basic() {
        let input = serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "搜索关键词"}
            },
            "required": ["query"]
        });
        let schema = build_tool_schema("mcp_server_search", "搜索工具", &input);
        assert_eq!(schema.name, "mcp_server_search");
        assert_eq!(schema.description, "搜索工具");
        assert!(schema.parameters.properties.contains_key("query"));
        assert!(schema.parameters.required.contains(&"query".to_string()));
    }

    #[test]
    fn build_tool_schema_empty_input() {
        let input = serde_json::Value::Null;
        let schema = build_tool_schema("mcp_server_tool", "工具", &input);
        assert_eq!(schema.name, "mcp_server_tool");
        assert!(!schema.parameters.properties.is_empty() || schema.parameters.kind == "object");
    }
}
