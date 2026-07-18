//! Session CRUD：创建 / 读取 / 更新 / 删除 / 列表 / 计数

use super::row::SessionRow;
use crate::error::SessionError;
use fuyao_api::Session;

impl super::SessionStore {
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
}
