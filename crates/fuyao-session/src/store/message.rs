//! 消息持久化助手（仅供本模块内部使用）：增量保存 / 加载会话消息

use super::row::MessageRow;
use crate::error::SessionError;
use fuyao_api::{Message, Session};

impl super::SessionStore {
    /// 增量保存消息（只插入新增部分）
    pub(super) async fn save_messages_tx(
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
    pub(super) async fn load_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<Message>, SessionError> {
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
