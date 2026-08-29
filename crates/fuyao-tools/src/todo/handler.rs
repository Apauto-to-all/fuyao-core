//! Todo 任务管理工具处理函数
//!
//! Agent 内部小循环的任务列表，用于拆解复杂任务、跟踪进度。
//! 长时间闭环运行时，todo 列表持久化到 session 数据库，不会丢失。
//!
//! 不传 todos 参数 = 读取当前列表，传了 = 整体覆盖写入。
//! session_id 由 runner 通过 ToolCallContext 注入，不由 LLM 传递。
//! 存储能力经 ctx.capabilities.todo_store 注入（SessionStore 实现的 TodoStoreOps），
//! 不再自建连接池。

use super::types::{TodoSummary, TodoWriteArgs, TodoWriteResult};
use crate::common::{parse_tool_args, to_ok_output};
use crate::config::TODO_MAX_ITEMS;
use fuyao_api::{CancellationToken, TodoItem, ToolCallContext, ToolError, ToolOutput};
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
///
/// 畸形项（id/content 缺失或为空）fail-loud 报错而非静默跳过——
/// 跳过会让 LLM 误以为全部写入成功，造成任务静默丢失。
/// status 容错保留：非法值回退 pending。
/// 数量上限兜底：单次超过 `TODO_MAX_ITEMS` 项即报错拒绝整批。
pub async fn todo_handler(
    args: Value,
    ctx: ToolCallContext,
    _cancel: CancellationToken,
) -> ToolOutput {
    let TodoWriteArgs { todos } = match parse_tool_args(args) {
        Ok(a) => a,
        Err(e) => return e,
    };

    let session_id = match &ctx.session_id {
        Some(id) => id,
        None => {
            return ToolOutput::error("缺少 session_id，请检查 Agent 是否正确初始化了 session");
        }
    };

    // 参数校验优先于存储取用：先把输入整体转换成 TodoItem（畸形项 fail-loud），
    // 校验失败即刻短路返回，再取 store——输入错误不应触碰任何 I/O
    let write_items = match todos.map(|inputs| {
        // 数量上限兜底：失控或注入式巨量提交时拒绝整批，防任务列表无界膨胀
        if inputs.len() > TODO_MAX_ITEMS {
            return Err(ToolError::new(format!(
                "todos 数量 {} 超过单次提交上限 {}，已拒绝整批写入",
                inputs.len(),
                TODO_MAX_ITEMS
            ))
            .with(
                "suggestion",
                "请精简任务列表：合并同类项或拆分为多次提交，每次不超过上限",
            ));
        }

        let mut items: Vec<TodoItem> = Vec::with_capacity(inputs.len());
        for raw in inputs {
            let item_id = raw.id.trim().to_string();
            let content = raw.content.trim().to_string();
            if item_id.is_empty() || content.is_empty() {
                return Err(
                    ToolError::new("todo 项的 id 与 content 均为必填且不能为空白").with(
                        "suggestion",
                        "请为每个任务提供非空的 id 与 content 后整体重发",
                    ),
                );
            }

            let status = if VALID_STATUSES.contains(&raw.status.as_str()) {
                raw.status
            } else {
                "pending".to_string()
            };

            items.push(TodoItem {
                id: item_id,
                content,
                status,
            });
        }
        Ok(items)
    }) {
        Some(Ok(items)) => Some(items),
        Some(Err(e)) => return ToolOutput::Err(e),
        None => None,
    };

    let store = match &ctx.capabilities.todo_store {
        Some(s) => s.clone(),
        None => {
            return ToolOutput::error(
                "任务列表存储未注入（TodoStoreOps 不可用），请检查工具调用上下文配置",
            );
        }
    };

    let result_items = match write_items {
        // 整体覆盖写入
        Some(items) => match store.write_todos(session_id, items).await {
            Ok(result) => result,
            Err(e) => return ToolOutput::error(format!("写入 todo 失败: {e}")),
        },
        // 读取
        None => match store.read_todos(session_id).await {
            Ok(result) => result,
            Err(e) => return ToolOutput::error(format!("读取 todo 失败: {e}")),
        },
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
        summary: build_summary(&result_items),
        todos,
    };

    to_ok_output(&result)
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
        let result = todo_handler(serde_json::json!({}), ctx, CancellationToken::new())
            .await
            .to_wire();
        assert!(result.contains("缺少 session_id"));
    }

    #[tokio::test]
    async fn todo_handler_returns_error_without_todo_store() {
        let ctx = ToolCallContext {
            session_id: Some("test".to_string()),
            ..ToolCallContext::default()
        };
        let result = todo_handler(serde_json::json!({}), ctx, CancellationToken::new())
            .await
            .to_wire();
        assert!(result.contains("TodoStoreOps 不可用"));
    }

    #[tokio::test]
    async fn todo_handler_returns_error_for_non_array_todos() {
        let ctx = ToolCallContext {
            session_id: Some("test".to_string()),
            ..ToolCallContext::default()
        };
        let result = todo_handler(
            serde_json::json!({ "todos": "not_array" }),
            ctx,
            CancellationToken::new(),
        )
        .await
        .to_wire();
        // 类型化解析：todos 非数组在反序列化时即报错
        assert!(result.contains("参数类型不正确"), "实际：{result}");
        assert!(result.contains("error"), "实际：{result}");
    }

    /// 畸形项（content 为空白）fail-loud：不再静默跳过，防止任务静默丢失
    #[tokio::test]
    async fn todo_handler_fails_loud_on_blank_item_field() {
        let ctx = ToolCallContext {
            session_id: Some("test".to_string()),
            ..ToolCallContext::default()
        };
        let result = todo_handler(
            serde_json::json!({ "todos": [{ "id": "1", "content": "  " }] }),
            ctx,
            CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("id 与 content 均为必填"));
        assert!(result.contains("suggestion"));
    }

    /// 数量超上限（201 项）整批拒绝，错误信息含实际上限值与精简建议
    #[tokio::test]
    async fn todo_handler_rejects_more_than_max_items() {
        let ctx = ToolCallContext {
            session_id: Some("test".to_string()),
            ..ToolCallContext::default()
        };
        let items: Vec<_> = (0..=TODO_MAX_ITEMS)
            .map(|i| serde_json::json!({ "id": format!("{i}"), "content": format!("任务{i}") }))
            .collect();
        assert_eq!(items.len(), TODO_MAX_ITEMS + 1);
        let result = todo_handler(
            serde_json::json!({ "todos": items }),
            ctx,
            CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("超过单次提交上限"), "实际：{result}");
        assert!(result.contains("精简"), "实际：{result}");
    }

    /// 恰好 200 项不触发数量上限：校验通过后走到存储层（未注入 store 报存储错误）
    #[tokio::test]
    async fn todo_handler_accepts_exactly_max_items() {
        let ctx = ToolCallContext {
            session_id: Some("test".to_string()),
            ..ToolCallContext::default()
        };
        let items: Vec<_> = (0..TODO_MAX_ITEMS)
            .map(|i| serde_json::json!({ "id": format!("{i}"), "content": format!("任务{i}") }))
            .collect();
        assert_eq!(items.len(), TODO_MAX_ITEMS);
        let result = todo_handler(
            serde_json::json!({ "todos": items }),
            ctx,
            CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("TodoStoreOps 不可用"), "实际：{result}");
    }
}
