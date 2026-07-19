//! 消息持久化助手（仅供本模块内部使用）：增量保存 / 加载会话消息
//!
//! seq 生成规则：插入时事务内 `SELECT COALESCE(MAX(seq), 0) + 1 FROM messages
//! WHERE session_id = ?`，事务保证并发安全。
//! 普通消息与 compaction 边界消息共享同一 seq 序列。

use crate::error::SessionError;
use fuyao_api::Session;

impl super::SessionStore {
    /// 增量保存消息（只插入新增部分）
    ///
    /// 约定：`msg.seq > 0` 视为已落库（跳过）；`msg.seq == 0` 视为新消息，分配
    /// `MAX(seq)+1` 起 INSERT，并把分配到的 seq 回填到内存对象，确保下次 update
    /// 不会重复插入。
    ///
    /// 这套约定能正确处理 compaction 后内存 messages 数组短于 DB 条数的情况——
    /// 只有真正新构造（seq=0）的消息才会被 INSERT。
    pub(super) async fn save_messages_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        session: &mut Session,
    ) -> Result<(), SessionError> {
        let mut next_seq: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM messages WHERE session_id = ?1")
                .bind(session.id.as_str())
                .fetch_one(&mut **tx)
                .await?;

        for msg in &mut session.messages {
            // 已落库的消息跳过
            if msg.seq > 0 {
                continue;
            }

            next_seq += 1;
            let tool_calls_json = msg
                .tool_calls
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default());

            sqlx::query(
                "INSERT INTO messages (session_id, model_id, role, content, tool_call_id,
                    tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                    reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
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
            .bind(next_seq)
            .bind(msg.kind.as_str())
            .execute(&mut **tx)
            .await?;

            // 回填到内存对象，下次 update 不会重复插
            msg.seq = next_seq;
        }

        Ok(())
    }
}
