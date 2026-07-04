//! Todo 任务管理工具处理函数
//!
//! Agent 内部小循环的任务列表，用于拆解复杂任务、跟踪进度。
//! 长时间闭环运行时，todo 列表持久化到 session 数据库，不会丢失。
//!
//! 不传 todos 参数 = 读取当前列表，传了 = 整体覆盖写入。
//! session_id 由 runner 通过 ToolCallContext 注入，不由 LLM 传递。

use super::types::{TodoSummary, TodoWriteResult};
use crate::common;
use fuyao_api::{TodoItem, ToolCallContext};
use fuyao_session::get_session_manager;
use serde_json::Value;

/// 合法的 status 值
const VALID_STATUSES: &[&str] = &["pending", "in_progress", "completed", "cancelled"];

/// 构建摘要统计
fn build_summary(items: &[TodoItem]) -> TodoSummary {
    let mut pending = 0usize;
    let mut in_progress = 0usize;
    let mut completed = 0usize;
    let mut cancelled = 0usize;

    for item in items {
        match item.status.as_str() {
            "pending" => pending += 1,
            "in_progress" => in_progress += 1,
            "completed" => completed += 1,
            "cancelled" => cancelled += 1,
            _ => pending += 1,
        }
    }

    TodoSummary {
        total: items.len(),
        pending,
        in_progress,
        completed,
        cancelled,
    }
}

/// Todo 工具处理函数
///
/// 不传 todos → 读取当前列表
/// 传 todos → 整体覆盖写入
pub async fn todo_handler(args: Value, ctx: &ToolCallContext) -> String {
    let session_id = match &ctx.session_id {
        Some(id) => id,
        None => {
            return common::tool_error("缺少 session_id，请检查 Agent 是否正确初始化了 session");
        }
    };

    let agent_paths = match &ctx.agent_paths {
        Some(paths) => paths,
        None => {
            return common::tool_error("TodoStore 需要 agent_paths，请检查 ToolCallContext 配置");
        }
    };

    let db_path = agent_paths.sessions_db_path();
    let manager = match get_session_manager(db_path).await {
        Ok(m) => m,
        Err(e) => return common::tool_error(&format!("获取 SessionManager 失败: {e}")),
    };
    let store = manager.get_todo_store();

    let todos_data = args.get("todos");

    let result_items = if let Some(todos) = todos_data {
        // 整体覆盖写入
        let arr = match todos.as_array() {
            Some(arr) => arr,
            None => return common::tool_error("todos 必须是数组"),
        };

        let mut items: Vec<TodoItem> = Vec::new();
        for raw in arr {
            let obj = match raw.as_object() {
                Some(obj) => obj,
                None => continue,
            };

            let item_id = match obj.get("id").and_then(|v| v.as_str()) {
                Some(id) => id.trim().to_string(),
                None => continue,
            };
            let content = match obj.get("content").and_then(|v| v.as_str()) {
                Some(c) => c.trim().to_string(),
                None => continue,
            };
            if item_id.is_empty() || content.is_empty() {
                continue;
            }

            let status = obj
                .get("status")
                .and_then(|v| v.as_str())
                .map(|s| {
                    if VALID_STATUSES.contains(&s) {
                        s.to_string()
                    } else {
                        "pending".to_string()
                    }
                })
                .unwrap_or_else(|| "pending".to_string());

            items.push(TodoItem {
                id: item_id,
                content,
                status,
            });
        }

        match store.write(session_id, items).await {
            Ok(result) => result,
            Err(e) => return common::tool_error(&format!("写入 todo 失败: {e}")),
        }
    } else {
        // 读取
        match store.read(session_id).await {
            Ok(result) => result,
            Err(e) => return common::tool_error(&format!("读取 todo 失败: {e}")),
        }
    };

    let todos: Vec<serde_json::Value> = result_items
        .iter()
        .map(|item| {
            serde_json::json!({
                "id": item.id,
                "content": item.content,
                "status": item.status,
            })
        })
        .collect();

    let result = TodoWriteResult {
        success: true,
        summary: Some(build_summary(&result_items)),
        todos,
        error: None,
    };

    common::tool_result(serde_json::to_value(result).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_summary_counts_correctly() {
        let items = vec![
            TodoItem {
                id: "1".to_string(),
                content: "任务1".to_string(),
                status: "pending".to_string(),
            },
            TodoItem {
                id: "2".to_string(),
                content: "任务2".to_string(),
                status: "in_progress".to_string(),
            },
            TodoItem {
                id: "3".to_string(),
                content: "任务3".to_string(),
                status: "completed".to_string(),
            },
            TodoItem {
                id: "4".to_string(),
                content: "任务4".to_string(),
                status: "cancelled".to_string(),
            },
        ];
        let summary = build_summary(&items);
        assert_eq!(summary.total, 4);
        assert_eq!(summary.pending, 1);
        assert_eq!(summary.in_progress, 1);
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.cancelled, 1);
    }

    #[test]
    fn build_summary_empty_list() {
        let items: Vec<TodoItem> = vec![];
        let summary = build_summary(&items);
        assert_eq!(summary.total, 0);
        assert_eq!(summary.pending, 0);
    }

    #[test]
    fn build_summary_unknown_status_defaults_to_pending() {
        let items = vec![TodoItem {
            id: "1".to_string(),
            content: "任务".to_string(),
            status: "unknown".to_string(),
        }];
        let summary = build_summary(&items);
        assert_eq!(summary.pending, 1);
    }

    #[tokio::test]
    async fn todo_handler_returns_error_without_session_id() {
        let ctx = ToolCallContext::default();
        let result = todo_handler(serde_json::json!({}), &ctx).await;
        assert!(result.contains("缺少 session_id"));
    }

    #[tokio::test]
    async fn todo_handler_returns_error_without_agent_paths() {
        let ctx = ToolCallContext {
            session_id: Some("test".to_string()),
            agent_paths: None,
        };
        let result = todo_handler(serde_json::json!({}), &ctx).await;
        assert!(result.contains("agent_paths"));
    }

    #[tokio::test]
    async fn todo_handler_returns_error_for_non_array_todos() {
        let ctx = ToolCallContext {
            session_id: Some("test".to_string()),
            agent_paths: Some(fuyao_api::AgentPaths::default()),
        };
        let result = todo_handler(serde_json::json!({ "todos": "not_array" }), &ctx).await;
        assert!(result.contains("todos 必须是数组"));
    }
}
