//! todos 表 CRUD
//!
//! 任务列表（todo）的全部操作归此：读取（按 sort_order 排序）、整体覆盖写入、
//! 按 session_id 删除全部（会话删除时级联清理）。
//!
//! todos 表按 session_id 软关联会话——`session_id` 仅作字符串过滤键，不加外键约束，
//! 数据隔离靠查询过滤。会话删除时由 [`super::SessionStore::delete`] 在同一事务内
//! 显式删 todos 行，保证无残留。

use crate::error::SessionError;
use fuyao_api::{TodoItem, TodoStoreOps};
use sqlx::Row;
use std::future::Future;
use std::pin::Pin;

impl super::SessionStore {
    /// 读取指定 session 的任务列表（按 sort_order 排序）
    pub async fn read_todos(&self, session_id: &str) -> Result<Vec<TodoItem>, SessionError> {
        let rows = sqlx::query(
            "SELECT id, content, status FROM todos WHERE session_id = ?1 ORDER BY sort_order, created_at",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        let items = rows
            .into_iter()
            .map(|row| TodoItem {
                id: row.get("id"),
                content: row.get("content"),
                status: row.get("status"),
            })
            .collect();

        Ok(items)
    }

    /// 整体覆盖写入指定 session 的任务列表
    ///
    /// 先删除该 session 的全部 todo，再按传入顺序重新插入。
    pub async fn write_todos(
        &self,
        session_id: &str,
        todos: Vec<TodoItem>,
    ) -> Result<Vec<TodoItem>, SessionError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        let mut tx = self.pool.begin().await?;

        sqlx::query("DELETE FROM todos WHERE session_id = ?1")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;

        for (i, todo) in todos.iter().enumerate() {
            sqlx::query(
                "INSERT INTO todos (id, session_id, content, status, sort_order, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .bind(todo.id.as_str())
            .bind(session_id)
            .bind(todo.content.as_str())
            .bind(todo.status.as_str())
            .bind(i as i64)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        self.read_todos(session_id).await
    }

    /// 删除指定 session 的全部任务（会话删除级联清理用）
    ///
    /// 不对外暴露为查询能力，仅供 [`delete`](super::SessionStore::delete) 在事务内调用，
    /// 保证删会话时任务列表无残留。
    pub(super) async fn delete_todos_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        session_id: &str,
    ) -> Result<(), SessionError> {
        sqlx::query("DELETE FROM todos WHERE session_id = ?1")
            .bind(session_id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
}

/// 会话存储实现任务列表能力接口
///
/// `SessionStore` 是 `sessions.db` 的唯一 owner，任务列表表与会话表同居一个数据库，
/// 故任务列表的读写能力直接由会话存储提供。经工具调用上下文（`ToolCallContext`）
/// 注入到 todo 工具，工具层不再自建连接池。
//
// trait 方法返 `Pin<Box<dyn Future>>` 是 async fn in dyn trait 的标准写法，
// 错误类型转 String 供调用方生成可读工具结果。
#[allow(clippy::type_complexity)]
impl TodoStoreOps for super::SessionStore {
    fn read_todos<'a>(
        &'a self,
        session_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<TodoItem>, String>> + Send + 'a>> {
        Box::pin(async move {
            super::SessionStore::read_todos(self, session_id)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn write_todos<'a>(
        &'a self,
        session_id: &'a str,
        todos: Vec<TodoItem>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<TodoItem>, String>> + Send + 'a>> {
        Box::pin(async move {
            super::SessionStore::write_todos(self, session_id, todos)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::SessionStore;
    use fuyao_api::TodoItem;

    /// 构造临时存储（独立临时目录）
    async fn temp_store() -> SessionStore {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let db_path = dir.path().join("test.db");
        // forget 让目录留到进程结束（async 测试里 SessionStore 跨 await 持有路径，dir 必须存活）
        std::mem::forget(dir);
        SessionStore::new(db_path).await.expect("创建存储失败")
    }

    #[tokio::test]
    async fn read_todos_returns_empty_for_new_session() {
        let store = temp_store().await;
        let items = store.read_todos("sess_none").await.unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let store = temp_store().await;
        let todos = vec![
            TodoItem {
                id: "1".to_string(),
                content: "任务1".to_string(),
                status: "pending".to_string(),
            },
            TodoItem {
                id: "2".to_string(),
                content: "任务2".to_string(),
                status: "completed".to_string(),
            },
        ];

        let result = store.write_todos("sess_a", todos).await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "任务1");
        assert_eq!(result[1].status, "completed");

        // 二次读取验证持久化
        let again = store.read_todos("sess_a").await.unwrap();
        assert_eq!(again.len(), 2);
        assert_eq!(again[0].id, "1");
    }

    #[tokio::test]
    async fn write_todos_overwrites_previous() {
        let store = temp_store().await;
        store
            .write_todos(
                "sess_b",
                vec![TodoItem {
                    id: "1".to_string(),
                    content: "旧任务".to_string(),
                    status: "pending".to_string(),
                }],
            )
            .await
            .unwrap();

        let result = store
            .write_todos(
                "sess_b",
                vec![TodoItem {
                    id: "2".to_string(),
                    content: "新任务".to_string(),
                    status: "in_progress".to_string(),
                }],
            )
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "新任务");
    }

    #[tokio::test]
    async fn write_todos_isolated_by_session_id() {
        let store = temp_store().await;
        store
            .write_todos(
                "sess_x",
                vec![TodoItem {
                    id: "1".to_string(),
                    content: "x 任务".to_string(),
                    status: "pending".to_string(),
                }],
            )
            .await
            .unwrap();
        store
            .write_todos(
                "sess_y",
                vec![TodoItem {
                    id: "1".to_string(),
                    content: "y 任务".to_string(),
                    status: "pending".to_string(),
                }],
            )
            .await
            .unwrap();

        let x = store.read_todos("sess_x").await.unwrap();
        let y = store.read_todos("sess_y").await.unwrap();
        assert_eq!(x[0].content, "x 任务");
        assert_eq!(y[0].content, "y 任务");
    }
}
