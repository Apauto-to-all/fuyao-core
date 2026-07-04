//! Todo 持久化存储
//!
//! 共享同一个 sessions.db 的 SqlitePool（WAL 模式安全），按 session_id 隔离。
//! 与 SessionManager 共享同一连接池（修复了原先各自打开独立连接的缺陷）。

use crate::error::SessionError;
use fuyao_api::TodoItem;
use sqlx::{Row, SqlitePool};

/// Todo 存储
pub struct TodoStore {
    pool: SqlitePool,
}

impl TodoStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 读取指定 session 的 todo 列表
    pub async fn read(&self, session_id: &str) -> Result<Vec<TodoItem>, SessionError> {
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

    /// 整体覆盖写入
    pub async fn write(
        &self,
        session_id: &str,
        todos: Vec<TodoItem>,
    ) -> Result<Vec<TodoItem>, SessionError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
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
        self.read(session_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SQLiteStore;
    use fuyao_api::Session;

    async fn setup() -> (SqlitePool, String) {
        let dir = std::env::temp_dir()
            .join("fuyao_todo_test")
            .join(uuid::Uuid::new_v4().to_string());
        let store = SQLiteStore::new(dir.join("test.db")).await.unwrap();
        let session = Session::new(None, None);
        let session_id = session.id.clone();
        store.create(&session).await.unwrap();
        (store.pool().clone(), session_id)
    }

    #[tokio::test]
    async fn todo_read_returns_empty_for_new_session() {
        let (pool, sid) = setup().await;
        let todo_store = TodoStore::new(pool);
        let items = todo_store.read(&sid).await.unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn todo_write_and_read() {
        let (pool, sid) = setup().await;
        let todo_store = TodoStore::new(pool);

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

        let result = todo_store.write(&sid, todos).await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "任务1");
        assert_eq!(result[1].status, "completed");
    }

    #[tokio::test]
    async fn todo_write_overwrites_previous() {
        let (pool, sid) = setup().await;
        let todo_store = TodoStore::new(pool);

        todo_store
            .write(
                &sid,
                vec![TodoItem {
                    id: "1".to_string(),
                    content: "旧任务".to_string(),
                    status: "pending".to_string(),
                }],
            )
            .await
            .unwrap();

        let result = todo_store
            .write(
                &sid,
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
}
