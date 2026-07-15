//! Todo 持久化存储
//!
//! 工具层自带的 todo 存储，与 SessionStore 共用同一个 `sessions.db` 文件
//! （SQLite WAL 模式下多个独立连接池同文件安全：读读并发、读写快照隔离、
//! 写写靠 busy_timeout 串行）。
//!
//! 不加外键约束——session_id 作为字符串软关联隔离数据，避免与 session 层
//! schema 耦合（todos 表属工具层职责，session 层不维护它）。

use fuyao_api::TodoItem;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::path::PathBuf;
use std::time::Duration;

/// todos 表 DDL
const TODOS_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS todos (
    id          TEXT NOT NULL,
    session_id  TEXT NOT NULL,
    content     TEXT NOT NULL,
    status      TEXT NOT NULL,
    sort_order  INTEGER NOT NULL,
    created_at  REAL NOT NULL,
    updated_at  REAL NOT NULL,
    PRIMARY KEY (session_id, id)
);

CREATE INDEX IF NOT EXISTS idx_todos_session ON todos(session_id, sort_order);
"#;

/// Todo 存储
///
/// 持有独立的 `SqlitePool`（指向 sessions.db），按 session_id 隔离各会话的 todo 列表。
pub struct TodoStore {
    pool: SqlitePool,
}

impl TodoStore {
    /// 创建并初始化存储
    ///
    /// 打开 `db_path` 指向的 SQLite 库（与 SessionStore 同一个文件），
    /// 建表后返回实例。连接参数（busy_timeout / max_connections）从全局配置
    /// `get_config().session.storage` 读取，与 SessionStore 保持一致。
    pub async fn new(db_path: PathBuf) -> Result<Self, sqlx::Error> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let storage = fuyao_api::get_config().session.storage.clone();

        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(storage.busy_timeout_secs));

        let pool = SqlitePoolOptions::new()
            .max_connections(storage.max_connections)
            .connect_with(options)
            .await?;

        sqlx::raw_sql(TODOS_SCHEMA_SQL).execute(&pool).await?;

        Ok(Self { pool })
    }

    /// 读取指定 session 的 todo 列表
    pub async fn read(&self, session_id: &str) -> Result<Vec<TodoItem>, sqlx::Error> {
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
    ///
    /// 先删除该 session 的全部 todo，再按传入顺序重新插入。
    pub async fn write(
        &self,
        session_id: &str,
        todos: Vec<TodoItem>,
    ) -> Result<Vec<TodoItem>, sqlx::Error> {
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
        self.read(session_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造临时存储（独立临时目录，测试结束自动清理）
    async fn temp_store() -> TodoStore {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let db_path = dir.path().join("test.db");
        // async 测试跨 await 持有路径，forget 让目录留到进程结束
        std::mem::forget(dir);
        TodoStore::new(db_path).await.expect("创建 TodoStore 失败")
    }

    #[tokio::test]
    async fn read_returns_empty_for_new_session() {
        let store = temp_store().await;
        let items = store.read("sess_none").await.unwrap();
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

        let result = store.write("sess_a", todos).await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "任务1");
        assert_eq!(result[1].status, "completed");

        // 二次读取验证持久化
        let again = store.read("sess_a").await.unwrap();
        assert_eq!(again.len(), 2);
        assert_eq!(again[0].id, "1");
    }

    #[tokio::test]
    async fn write_overwrites_previous() {
        let store = temp_store().await;
        store
            .write(
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
            .write(
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
    async fn sessions_isolated_by_session_id() {
        let store = temp_store().await;
        store
            .write(
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
            .write(
                "sess_y",
                vec![TodoItem {
                    id: "1".to_string(),
                    content: "y 任务".to_string(),
                    status: "pending".to_string(),
                }],
            )
            .await
            .unwrap();

        let x = store.read("sess_x").await.unwrap();
        let y = store.read("sess_y").await.unwrap();
        assert_eq!(x[0].content, "x 任务");
        assert_eq!(y[0].content, "y 任务");
    }
}
