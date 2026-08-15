//! MCP 工具桥接
//!
//! 将 MCP Server 暴露的工具构建为 (name, schema, handler) 三元组，
//! 由调用方决定如何注册。
//!
//! 集成：超时保护、熔断器、Auth 恢复、Session 恢复。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use fuyao_api::{CancellationToken, ToolCallContext, ToolError, ToolFn, ToolOutput};
use rmcp::model::CallToolResult;
use serde_json::Value;

use crate::circuit_breaker::CircuitBreaker;
use crate::connection::MCPConnection;
use crate::security::{sanitize_error, sanitize_mcp_name_component};

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

/// 从 CallToolResult 提取错误文本（is_error=true 时用）
///
/// 收集所有文本内容片段拼接为完整错误描述。
pub(crate) fn extract_error_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<&str>>()
        .join("")
}

/// 从 CallToolResult 提取文本与结构化内容，组装为统一的结果对象
///
/// 规则：有 structuredContent 时并入 result；否则仅返回 result 文本。
/// 统一 call_tool（管理器直调）与 do_call（handler 路径）的结果形态。
pub(crate) fn extract_call_output(result: &CallToolResult) -> Value {
    let parts: Vec<String> = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect();
    let text_result = parts.join("\n");

    if let Some(structured) = &result.structured_content {
        if !text_result.is_empty() {
            serde_json::json!({"result": text_result, "structuredContent": structured})
        } else {
            serde_json::json!({"result": structured})
        }
    } else {
        serde_json::json!({"result": text_result})
    }
}

/// 构建带 MCPConnection 引用的工具 handler
///
/// 由于 ToolFn 需要是 'static 的，通过 `Arc<Mutex<Option<MCPConnection>>>` 传递连接，
/// 熔断器状态经 `Arc<CircuitBreaker>` 共享给同一 manager 下的所有工具。
pub fn make_tool_call_handler(
    connection: Arc<tokio::sync::Mutex<Option<MCPConnection>>>,
    breaker: Arc<CircuitBreaker>,
    tool_name: String,
    server_name: String,
    tool_timeout: u32,
) -> ToolFn {
    Arc::new(
        move |args: Value, _ctx: ToolCallContext, _cancel: CancellationToken| {
            let tool_name = tool_name.clone();
            let server_name = server_name.clone();
            let timeout_secs = tool_timeout;
            let conn = connection.clone();
            let breaker = breaker.clone();

            Box::pin(async move {
                // 调用入口留痕：一被发起就记录，立即能区分「handler 没被调起」
                // （无此日志 = 上游 tool_exec 的问题）vs「handler 在执行中卡住」
                // （有此日志但长时间无完成日志 = MCP server / rmcp 卡住）
                tracing::info!(
                    server = %server_name,
                    tool = %tool_name,
                    timeout_secs = timeout_secs,
                    "MCP 工具调用开始"
                );

                // 检查熔断器
                if let Some(msg) = breaker.check_breaker(&server_name) {
                    tracing::warn!(
                        server = %server_name,
                        tool = %tool_name,
                        reason = %msg,
                        "MCP 工具调用被熔断器拦截"
                    );
                    return ToolOutput::error(msg);
                }

                // 执行 MCP 调用
                let started = std::time::Instant::now();
                let call_result = tokio::time::timeout(
                    Duration::from_secs(timeout_secs as u64),
                    do_call(&conn, &tool_name, args.clone()),
                )
                .await;

                let output = match call_result {
                    Ok(Ok(output)) => {
                        if output.is_error() {
                            breaker.bump_error(&server_name);
                        } else {
                            breaker.reset_error(&server_name);
                        }
                        output
                    }
                    Ok(Err(err_msg)) => {
                        // 检测可恢复错误（Auth/Session）并尝试重连重试
                        if let Some(recovered) = try_recover_and_retry(
                            &conn,
                            &breaker,
                            &server_name,
                            &tool_name,
                            &err_msg,
                            &args,
                            started,
                        )
                        .await
                        {
                            return recovered;
                        }
                        breaker.bump_error(&server_name);
                        ToolOutput::Err(ToolError::new(sanitize_error(&format!(
                            "MCP 调用失败: {err_msg}"
                        ))))
                    }
                    Err(_) => {
                        // 超时是可恢复故障，按日志规范记 WARN（突出故障态，与下方通用完成 INFO 互补）
                        tracing::warn!(
                            server = %server_name,
                            tool = %tool_name,
                            timeout_secs,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "MCP 工具调用超时"
                        );
                        breaker.bump_error(&server_name);
                        ToolOutput::error(format!(
                            "MCP tool '{tool_name}' timed out after {timeout_secs}s"
                        ))
                    }
                };

                tracing::info!(
                    name = %server_name,
                    tool = %tool_name,
                    ok = !output.is_error(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "MCP 工具调用完成"
                );
                output
            })
        },
    )
}

/// 执行单次 MCP 工具调用，返回结果信封
async fn do_call(
    conn: &Arc<tokio::sync::Mutex<Option<MCPConnection>>>,
    tool_name: &str,
    args: Value,
) -> Result<ToolOutput, String> {
    let guard = conn.lock().await;
    let conn = guard
        .as_ref()
        .ok_or_else(|| "MCP server 未连接".to_string())?;

    let result = conn
        .call_tool(tool_name, args)
        .await
        .map_err(|e| e.to_string())?;

    // 处理 MCP 调用结果：is_error 时以错误信封返回（handler 侧据此计入熔断）
    if result.is_error.unwrap_or(false) {
        let error_text = extract_error_text(&result);
        return Ok(ToolOutput::Err(ToolError::new(sanitize_error(&error_text))));
    }

    Ok(ToolOutput::ok(extract_call_output(&result)))
}

/// 对可恢复错误（Auth/Session）触发重连并重试，成功返回 Some(结果信封)
///
/// 两条恢复路径（鉴权失败、会话过期）结构相同，仅错误分类器不同，
/// 此处合并为一条：归类 → 留痕 → 通知重连 → 等待恢复后重试。
async fn try_recover_and_retry(
    conn: &Arc<tokio::sync::Mutex<Option<MCPConnection>>>,
    breaker: &CircuitBreaker,
    server_name: &str,
    tool_name: &str,
    err_msg: &str,
    args: &Value,
    started: std::time::Instant,
) -> Option<ToolOutput> {
    // 归类可恢复错误：auth 或 session，其余不处理
    let kind = if crate::recovery::is_auth_error_str(err_msg) {
        "auth"
    } else if crate::recovery::is_session_expired_error_str(err_msg) {
        "session"
    } else {
        return None;
    };

    tracing::warn!(
        server = %server_name,
        tool = %tool_name,
        kind,
        attempt = 1u8,
        recovered = false,
        cause = %err_msg,
        "MCP 调用遇到可恢复错误，触发重连"
    );
    notify_reconnect(conn);
    let retry_result = wait_and_retry(conn, server_name, tool_name, args).await?;
    breaker.reset_error(server_name);
    tracing::info!(
        name = %server_name,
        tool = %tool_name,
        ok = !retry_result.is_error(),
        recovered = true,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "MCP 工具调用完成"
    );
    Some(retry_result)
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
    server_name: &str,
    tool_name: &str,
    args: &Value,
) -> Option<ToolOutput> {
    // 等待 session 恢复（时长从全局配置 get_config().mcp 读取）
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs(fuyao_api::get_config().mcp.session_recovery_wait_secs);
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // 重试调用（attempt=2 表示这是第二次尝试）
    match do_call(conn, tool_name, args.clone()).await {
        Ok(output) => {
            if !output.is_error() {
                Some(output)
            } else {
                None
            }
        }
        Err(e) => {
            tracing::warn!(
                server = %server_name,
                tool = %tool_name,
                attempt = 2u8,
                recovered = false,
                cause = %e,
                "MCP 恢复后重试仍失败"
            );
            None
        }
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
}
