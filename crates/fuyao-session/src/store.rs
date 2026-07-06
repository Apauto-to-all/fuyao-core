//! SQLite 存储层
//!
//! 使用 sqlx（async）+ SqlitePool 连接池。
//! 原生 async，无需 spawn_blocking 包装。

use crate::error::SessionError;
use crate::schema::{SCHEMA_SQL, SCHEMA_VERSION};
use fuyao_api::{Message, Session};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{FromRow, SqlitePool};
use std::path::PathBuf;
use std::time::Duration;

/// 会话行（数据库列映射，字段顺序与 sessions 表一致）
#[derive(FromRow)]
struct SessionRow {
    id: String,
    parent_session_id: Option<String>,
    started_at: f64,
    ended_at: Option<f64>,
    end_reason: Option<String>,
    message_count: i64,
    tool_call_count: i64,
    total_prompt_tokens: i64,
    total_completion_tokens: i64,
    total_reasoning_tokens: i64,
    total_cached_tokens: i64,
    total_cost: f64,
    title: Option<String>,
    system_prompt: Option<String>,
}

impl From<SessionRow> for Session {
    fn from(r: SessionRow) -> Self {
        Session {
            id: r.id,
            parent_session_id: r.parent_session_id,
            title: r.title,
            system_prompt: r.system_prompt,
            message_count: r.message_count,
            tool_call_count: r.tool_call_count,
            total_prompt_tokens: r.total_prompt_tokens,
            total_completion_tokens: r.total_completion_tokens,
            total_reasoning_tokens: r.total_reasoning_tokens,
            total_cached_tokens: r.total_cached_tokens,
            total_cost: r.total_cost,
            started_at: r.started_at,
            ended_at: r.ended_at,
            end_reason: r.end_reason,
            messages: Vec::new(),
        }
    }
}

/// 消息行（数据库列映射，字段顺序与 messages 表一致）
#[derive(FromRow)]
struct MessageRow {
    id: Option<i64>,
    session_id: String,
    model_id: Option<String>,
    role: String,
    content: Option<String>,
    tool_call_id: Option<String>,
    tool_calls: Option<String>,
    tool_name: Option<String>,
    timestamp: f64,
    prompt_tokens: i64,
    completion_tokens: i64,
    reasoning_tokens: i64,
    cached_tokens: i64,
    cost: f64,
    finish_reason: Option<String>,
    reasoning: Option<String>,
}

impl From<MessageRow> for Message {
    fn from(r: MessageRow) -> Self {
        let tool_calls = r.tool_calls.and_then(|s| serde_json::from_str(&s).ok());
        Message {
            id: r.id,
            session_id: r.session_id,
            model_id: r.model_id,
            role: r.role,
            content: r.content,
            reasoning: r.reasoning,
            tool_call_id: r.tool_call_id,
            tool_calls,
            tool_name: r.tool_name,
            finish_reason: r.finish_reason,
            timestamp: r.timestamp,
            prompt_tokens: r.prompt_tokens,
            completion_tokens: r.completion_tokens,
            reasoning_tokens: r.reasoning_tokens,
            cached_tokens: r.cached_tokens,
            cost: r.cost,
        }
    }
}

/// SQLite 存储层
pub struct SQLiteStore {
    db_path: PathBuf,
    pool: SqlitePool,
}

impl SQLiteStore {
    /// 创建并初始化存储
    ///
    /// 连接参数（busy_timeout / max_connections）从全局配置 `get_config().session.storage` 读取。
    pub async fn new(db_path: PathBuf) -> Result<Self, SessionError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let storage = fuyao_api::get_config().session.storage.clone();

        // 连接选项：启用 WAL、外键、忙等待
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

        // 初始化 schema
        sqlx::raw_sql(SCHEMA_SQL).execute(&pool).await?;

        // 写入 schema 版本（仅首次）
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schema_version")
            .fetch_one(&pool)
            .await?;
        if count == 0 {
            sqlx::query("INSERT INTO schema_version (version) VALUES (?1)")
                .bind(SCHEMA_VERSION)
                .execute(&pool)
                .await?;
        }

        Ok(Self { db_path, pool })
    }

    /// 数据库文件路径
    pub fn db_path(&self) -> &PathBuf {
        &self.db_path
    }

    /// 获取连接池引用（供 TodoStore 共享同一连接池）
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// 创建会话
    pub async fn create(&self, session: &Session) -> Result<(), SessionError> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO sessions (id, parent_session_id, started_at, ended_at, end_reason,
                message_count, tool_call_count, total_prompt_tokens, total_completion_tokens,
                total_reasoning_tokens, total_cached_tokens, total_cost, title, system_prompt)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        )
        .bind(session.id.as_str())
        .bind(session.parent_session_id.as_deref())
        .bind(session.started_at)
        .bind(session.ended_at)
        .bind(session.end_reason.as_deref())
        .bind(session.message_count)
        .bind(session.tool_call_count)
        .bind(session.total_prompt_tokens)
        .bind(session.total_completion_tokens)
        .bind(session.total_reasoning_tokens)
        .bind(session.total_cached_tokens)
        .bind(session.total_cost)
        .bind(session.title.as_deref())
        .bind(session.system_prompt.as_deref())
        .execute(&mut *tx)
        .await?;

        Self::save_messages_tx(&mut tx, session).await?;
        tx.commit().await?;
        Ok(())
    }

    /// 获取会话（含消息）
    pub async fn get(&self, session_id: &str) -> Result<Option<Session>, SessionError> {
        let row = sqlx::query_as::<_, SessionRow>(
            "SELECT id, parent_session_id, started_at, ended_at, end_reason,
                    message_count, tool_call_count, total_prompt_tokens, total_completion_tokens,
                    total_reasoning_tokens, total_cached_tokens, total_cost, title, system_prompt
             FROM sessions WHERE id = ?1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(r) => {
                let mut session: Session = r.into();
                session.messages = self.load_messages(session_id).await?;
                Ok(Some(session))
            }
            None => Ok(None),
        }
    }

    /// 更新会话（增量保存消息）
    pub async fn update(&self, session: &Session) -> Result<(), SessionError> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "UPDATE sessions SET
                parent_session_id = ?2, ended_at = ?3, end_reason = ?4,
                message_count = ?5, tool_call_count = ?6,
                total_prompt_tokens = ?7, total_completion_tokens = ?8,
                total_reasoning_tokens = ?9, total_cached_tokens = ?10,
                total_cost = ?11, title = ?12, system_prompt = ?13
             WHERE id = ?1",
        )
        .bind(session.id.as_str())
        .bind(session.parent_session_id.as_deref())
        .bind(session.ended_at)
        .bind(session.end_reason.as_deref())
        .bind(session.message_count)
        .bind(session.tool_call_count)
        .bind(session.total_prompt_tokens)
        .bind(session.total_completion_tokens)
        .bind(session.total_reasoning_tokens)
        .bind(session.total_cached_tokens)
        .bind(session.total_cost)
        .bind(session.title.as_deref())
        .bind(session.system_prompt.as_deref())
        .execute(&mut *tx)
        .await?;

        Self::save_messages_tx(&mut tx, session).await?;
        tx.commit().await?;
        Ok(())
    }

    /// 删除会话
    pub async fn delete(&self, session_id: &str) -> Result<bool, SessionError> {
        sqlx::query("DELETE FROM todos WHERE session_id = ?1")
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM messages WHERE session_id = ?1")
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        let result = sqlx::query("DELETE FROM sessions WHERE id = ?1")
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 列出会话（不含消息，分页）
    pub async fn list_all(&self, limit: i64, offset: i64) -> Result<Vec<Session>, SessionError> {
        let rows = sqlx::query_as::<_, SessionRow>(
            "SELECT id, parent_session_id, started_at, ended_at, end_reason,
                    message_count, tool_call_count, total_prompt_tokens, total_completion_tokens,
                    total_reasoning_tokens, total_cached_tokens, total_cost, title, system_prompt
             FROM sessions ORDER BY started_at DESC LIMIT ?1 OFFSET ?2",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Session::from).collect())
    }

    /// 获取会话总数
    pub async fn count(&self) -> Result<i64, SessionError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// 增量保存消息（只插入新增部分）
    async fn save_messages_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        session: &Session,
    ) -> Result<(), SessionError> {
        let existing: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE session_id = ?1")
                .bind(session.id.as_str())
                .fetch_one(&mut **tx)
                .await?;

        let new_messages = &session.messages[existing as usize..];

        for msg in new_messages {
            let tool_calls_json = msg
                .tool_calls
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            sqlx::query(
                "INSERT INTO messages (session_id, model_id, role, content, tool_call_id,
                    tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                    reasoning_tokens, cached_tokens, cost, finish_reason, reasoning)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            )
            .bind(session.id.as_str())
            .bind(msg.model_id.as_deref())
            .bind(msg.role.as_str())
            .bind(msg.content.as_deref())
            .bind(msg.tool_call_id.as_deref())
            .bind(tool_calls_json.as_deref())
            .bind(msg.tool_name.as_deref())
            .bind(msg.timestamp)
            .bind(msg.prompt_tokens)
            .bind(msg.completion_tokens)
            .bind(msg.reasoning_tokens)
            .bind(msg.cached_tokens)
            .bind(msg.cost)
            .bind(msg.finish_reason.as_deref())
            .bind(msg.reasoning.as_deref())
            .execute(&mut **tx)
            .await?;
        }

        Ok(())
    }

    /// 加载会话的所有消息
    async fn load_messages(&self, session_id: &str) -> Result<Vec<Message>, SessionError> {
        let rows = sqlx::query_as::<_, MessageRow>(
            "SELECT id, session_id, model_id, role, content, tool_call_id,
                    tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                    reasoning_tokens, cached_tokens, cost, finish_reason, reasoning
             FROM messages WHERE session_id = ?1 ORDER BY timestamp",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Message::from).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::{Message, Session};

    async fn temp_store() -> SQLiteStore {
        let dir = std::env::temp_dir()
            .join("fuyao_session_test")
            .join(uuid::Uuid::new_v4().to_string());
        SQLiteStore::new(dir.join("test.db")).await.unwrap()
    }

    #[tokio::test]
    async fn store_create_and_get() {
        let store = temp_store().await;
        let mut session = Session::new(Some("测试".to_string()), None);
        session.messages.push(Message::user("你好".to_string()));

        store.create(&session).await.unwrap();
        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.title, Some("测试".to_string()));
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].content, Some("你好".to_string()));
    }

    #[tokio::test]
    async fn store_get_returns_none_for_missing() {
        let store = temp_store().await;
        let result = store.get("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn store_update_increments_messages() {
        let store = temp_store().await;
        let mut session = Session::new(None, None);
        session.messages.push(Message::user("第一条".to_string()));
        store.create(&session).await.unwrap();

        session
            .messages
            .push(Message::assistant(Some("回复".to_string())));
        session.message_count = 2;
        store.update(&session).await.unwrap();

        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[1].role, "assistant");
    }

    #[tokio::test]
    async fn store_delete_removes_session() {
        let store = temp_store().await;
        let session = Session::new(None, None);
        store.create(&session).await.unwrap();

        let deleted = store.delete(&session.id).await.unwrap();
        assert!(deleted);

        assert!(store.get(&session.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn store_list_all_returns_sessions() {
        let store = temp_store().await;
        let s1 = Session::new(Some("会话1".to_string()), None);
        let s2 = Session::new(Some("会话2".to_string()), None);
        store.create(&s1).await.unwrap();
        store.create(&s2).await.unwrap();

        let list = store.list_all(10, 0).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].title, Some("会话2".to_string()));
    }

    #[tokio::test]
    async fn store_count_returns_correct_count() {
        let store = temp_store().await;
        assert_eq!(store.count().await.unwrap(), 0);
        store.create(&Session::new(None, None)).await.unwrap();
        assert_eq!(store.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn store_incremental_save_only_inserts_new() {
        let store = temp_store().await;
        let mut session = Session::new(None, None);
        session.messages.push(Message::user("m1".to_string()));
        session.messages.push(Message::user("m2".to_string()));
        store.create(&session).await.unwrap();

        session.messages.push(Message::user("m3".to_string()));
        store.update(&session).await.unwrap();

        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 3);
        assert_eq!(loaded.messages[2].content, Some("m3".to_string()));
    }

    #[tokio::test]
    async fn store_tool_calls_serialized_as_json() {
        let store = temp_store().await;
        let mut session = Session::new(None, None);
        let mut msg = Message::assistant(None);
        msg.tool_calls = Some(serde_json::json!([{
            "id": "call_1",
            "type": "function",
            "function": { "name": "bash", "arguments": "{}" }
        }]));
        session.messages.push(msg);
        store.create(&session).await.unwrap();

        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert!(loaded.messages[0].tool_calls.is_some());
        assert_eq!(
            loaded.messages[0].tool_calls.as_ref().unwrap()[0]["id"],
            "call_1"
        );
    }
}
