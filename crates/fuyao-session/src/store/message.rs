//! 消息持久化助手（单条 INSERT 入口）
//!
//! seq 生成规则：插入时事务内 `SELECT COALESCE(MAX(seq), 0) + 1 FROM messages
//! WHERE session_id = ?`，事务保证并发安全。
//! 普通消息与 compaction 边界消息共享同一 seq 序列。
//!
//! 设计要点：消息产生即落库（事件级落库），不进任何内存数组——
//! 单个 session 的内存占用恒定（不随历史增长），多 session 并发无内存压力。
//! 需要历史消息时（构造 ChatRequest / 压缩 / 标题生成 / 中断补发等）
//! 通过 [`SessionStore::load_visible_messages`] 按需查询。

use crate::error::SessionError;
use fuyao_api::Message;

impl super::SessionStore {
    /// 单条消息落库：分配 seq 并 INSERT，回填到 msg.seq
    ///
    /// 这是消息进 DB 的唯一入口（事件级落库），所有产出消息（user / assistant /
    /// tool_result / 中断补发 / 工具执行结果）必经此入口。
    ///
    /// 事务内 `SELECT COALESCE(MAX(seq), 0) + 1` 分配新 seq，保证并发安全；
    /// INSERT 后回填 `msg.seq`，让调用方能继续使用（如返回到事件 payload）。
    ///
    /// 与原批量接口 `save_messages_tx` 的差异：
    /// - 不依赖 `session.messages` 内存数组（字段已删除）
    /// - 单条而非批量（每条消息产生时立即调用）
    /// - 调用方不再需要「turn 结束时统一 persist」——消息已经在了
    pub async fn insert_message(
        &self,
        session_id: &str,
        msg: &mut Message,
    ) -> Result<i64, SessionError> {
        let mut tx = self.pool.begin().await?;

        let next_seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE session_id = ?1",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;

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
        .bind(session_id)
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
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        msg.seq = next_seq;
        Ok(next_seq)
    }

    /// 统计 session 的消息总数（用于元数据 message_count 维护）
    ///
    /// 走 `idx_messages_session` 索引，O(1) 量级。
    pub async fn count_messages(&self, session_id: &str) -> Result<i64, SessionError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND kind = 'message'",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }
}
