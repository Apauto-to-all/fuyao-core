//! Session CRUD：创建 / 读取 / 更新 / 删除 / 列表 / 计数

use super::row::SessionRow;
use crate::error::SessionError;
use fuyao_api::Session;

impl super::SessionStore {
    /// 创建会话
    pub async fn create(&self, session: &mut Session) -> Result<(), SessionError> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO sessions (id, started_at, ended_at, end_reason,
                message_count, tool_call_count, total_prompt_tokens, total_completion_tokens,
                total_reasoning_tokens, total_cached_tokens, total_cost, title, system_prompt,
                compression_count, last_compacted_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )
        .bind(session.id.as_str())
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
        .bind(session.compression_count)
        .bind(session.last_compacted_seq)
        .execute(&mut *tx)
        .await?;

        Self::save_messages_tx(&mut tx, session).await?;
        tx.commit().await?;
        Ok(())
    }

    /// 获取会话（含可见窗口消息）
    ///
    /// 内部调 [`load_visible_messages`](super::SessionStore::load_visible_messages)：
    /// 内存 Session 持有的就是模型可见窗口（最近一条 compaction 及之后），
    /// 旧消息仍存 DB 但不进内存。
    pub async fn get(&self, session_id: &str) -> Result<Option<Session>, SessionError> {
        let row = sqlx::query_as::<_, SessionRow>(
            "SELECT id, started_at, ended_at, end_reason,
                    message_count, tool_call_count, total_prompt_tokens, total_completion_tokens,
                    total_reasoning_tokens, total_cached_tokens, total_cost, title, system_prompt,
                    compression_count, last_compacted_seq
             FROM sessions WHERE id = ?1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(r) => {
                let mut session: Session = r.into();
                session.messages = self.load_visible_messages(session_id).await?;
                Ok(Some(session))
            }
            None => Ok(None),
        }
    }

    /// 更新会话（增量保存消息）
    pub async fn update(&self, session: &mut Session) -> Result<(), SessionError> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "UPDATE sessions SET
                ended_at = ?2, end_reason = ?3,
                message_count = ?4, tool_call_count = ?5,
                total_prompt_tokens = ?6, total_completion_tokens = ?7,
                total_reasoning_tokens = ?8, total_cached_tokens = ?9,
                total_cost = ?10, title = ?11, system_prompt = ?12,
                compression_count = ?13, last_compacted_seq = ?14
             WHERE id = ?1",
        )
        .bind(session.id.as_str())
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
        .bind(session.compression_count)
        .bind(session.last_compacted_seq)
        .execute(&mut *tx)
        .await?;

        Self::save_messages_tx(&mut tx, session).await?;
        tx.commit().await?;
        Ok(())
    }

    /// 删除会话
    pub async fn delete(&self, session_id: &str) -> Result<bool, SessionError> {
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
            "SELECT id, started_at, ended_at, end_reason,
                    message_count, tool_call_count, total_prompt_tokens, total_completion_tokens,
                    total_reasoning_tokens, total_cached_tokens, total_cost, title, system_prompt,
                    compression_count, last_compacted_seq
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
}
