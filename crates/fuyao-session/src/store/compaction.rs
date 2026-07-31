//! 上下文压缩专用持久化（mark_compaction / load_visible_messages / load_full_history）
//!
//! 设计要点（对齐 1.4/1.6 设计决策）：
//! - session_id 永不变，压缩不创建新会话
//! - 压缩 = 插入一条 `kind='compaction'` 边界消息 + 更新 sessions 元数据
//! - 模型可见窗口 = 最近一条 compaction 消息及之后的所有消息（SQL `seq >=` 过滤）
//! - 旧消息物理保留（不删除、不归档），可审计可恢复
//! - 统计字段（token 累计）压缩不重置

use super::row::MessageRow;
use crate::error::SessionError;
use fuyao_api::Message;

/// 压缩触发原因（写入 compaction 消息时附带，便于审计）
#[derive(Debug, Clone, Copy)]
pub enum CompressionReason {
    /// 阈值自动触发
    Auto,
    /// 用户手动触发
    Manual,
    /// Provider overflow 错误后被动触发
    Overflow,
}

impl CompressionReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
            Self::Overflow => "overflow",
        }
    }
}

impl super::SessionStore {
    /// 标记一次压缩完成
    ///
    /// 事务内做三件事：
    /// 1. 给 messages 表插入一条 `kind='compaction'` 的边界消息（content=summary）
    /// 2. 更新 sessions.`last_compacted_seq` = 新 compaction 消息的 seq
    /// 3. sessions.`compression_count` += 1
    ///
    /// 不创建新 session、不动 session_id、不重置统计字段、不动 end_reason/ended_at。
    ///
    /// # 参数
    /// - `session_id`：被压缩的会话（永不变）
    /// - `summary`：摘要正文（Markdown），存入 compaction 消息的 content
    /// - `reason`：压缩触发原因
    ///
    /// # 返回
    /// 新 compaction 消息的 seq（同时也成为 `last_compacted_seq`）
    ///
    /// # 错误
    /// - [`SessionError::NotFound`]：session_id 在数据库中不存在
    pub async fn mark_compaction(
        &self,
        session_id: &str,
        summary: String,
        reason: CompressionReason,
    ) -> Result<i64, SessionError> {
        let mut tx = self.pool.begin().await?;

        // 验证 session 存在（避免给不存在的 session 插孤儿消息）
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)")
                .bind(session_id)
                .fetch_one(&mut *tx)
                .await?;
        if !exists {
            return Err(SessionError::NotFound(session_id.to_string()));
        }

        // 生成新 seq（事务内 COALESCE 保证并发安全）
        let next_seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE session_id = ?1",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        // 插入 compaction 边界消息（role='system' 避免与对话流混淆，kind='compaction' 是真标记）
        sqlx::query(
            "INSERT INTO messages (session_id, model_id, role, content, images, tool_call_id,
                tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind)
             VALUES (?1, NULL, 'system', ?2, NULL, NULL, NULL, ?3, ?4, 0, 0, 0, 0, 0, NULL, NULL, ?5, 'compaction')",
        )
        .bind(session_id)
        .bind(&summary)
        .bind(reason.as_str())
        .bind(now)
        .bind(next_seq)
        .execute(&mut *tx)
        .await?;

        // 更新 sessions 压缩元数据
        sqlx::query(
            "UPDATE sessions SET last_compacted_seq = ?2, compression_count = compression_count + 1
             WHERE id = ?1",
        )
        .bind(session_id)
        .bind(next_seq)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        tracing::info!(
            session_id = session_id,
            new_seq = next_seq,
            reason = reason.as_str(),
            "上下文压缩边界已落库"
        );
        Ok(next_seq)
    }

    /// 更新 session 的 system_prompt（压缩后重建系统提示词用）
    ///
    /// 单字段 UPDATE，不动其他字段、不动 messages 表。压缩触发后由 react 层调用，
    /// 把 `build_system_prompt` 重新构建的提示词落库，避免旧 system_prompt 中残留的
    /// 动态内容（如"基于刚才的 X 错误继续排查"）在 X 已被压进摘要后误导模型。
    ///
    /// # 错误
    /// - [`SessionError::NotFound`]：session_id 在数据库中不存在
    pub async fn update_system_prompt(
        &self,
        session_id: &str,
        new_system_prompt: &str,
    ) -> Result<(), SessionError> {
        let mut tx = self.pool.begin().await?;

        // 校验 session 存在（与 mark_compaction 一致的防护，避免给不存在的 session 写脏数据）
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)")
                .bind(session_id)
                .fetch_one(&mut *tx)
                .await?;
        if !exists {
            return Err(SessionError::NotFound(session_id.to_string()));
        }

        sqlx::query("UPDATE sessions SET system_prompt = ?2 WHERE id = ?1")
            .bind(session_id)
            .bind(new_system_prompt)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        tracing::info!(
            session_id = session_id,
            prompt_len = new_system_prompt.len(),
            "system_prompt 已更新（压缩后重建）"
        );
        Ok(())
    }

    /// 更新 session 的 title（标题自动生成后异步落库）
    ///
    /// 单字段 UPDATE，不动其他字段、不动 messages 表。由 react 层 fire-and-forget
    /// spawn 的标题生成任务调用——spawn 的 future 是 `'static` 的，无法借用 `&mut Session`，
    /// 故走单字段 SQL 而非全量 `update(session)`。
    ///
    /// # 错误
    /// - [`SessionError::NotFound`]：session_id 在数据库中不存在
    pub async fn update_title(
        &self,
        session_id: &str,
        new_title: &str,
    ) -> Result<(), SessionError> {
        let mut tx = self.pool.begin().await?;

        // 校验 session 存在（与 update_system_prompt 一致，避免给不存在的 session 写脏数据）
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)")
                .bind(session_id)
                .fetch_one(&mut *tx)
                .await?;
        if !exists {
            return Err(SessionError::NotFound(session_id.to_string()));
        }

        sqlx::query("UPDATE sessions SET title = ?2 WHERE id = ?1")
            .bind(session_id)
            .bind(new_title)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        tracing::info!(
            session_id = session_id,
            title = new_title,
            "会话标题已更新（自动生成）"
        );
        Ok(())
    }

    /// 标记会话结束（填 ended_at + end_reason）
    ///
    /// 单字段 UPDATE，不动其他字段、不动 messages 表。由 `Engine::end_session` 在
    /// session task 退出**之后**调用——确保 `ended_at` / `end_reason` 是最终值，
    /// 不被 task 退出时的全量 `update(&session)` 覆盖。
    ///
    /// 设计上与 `update_title` / `update_system_prompt` 对称：都是单字段更新方法，
    /// 走独立 SQL 路径而非全量 `update(session)`，避免并发更新间的字段覆盖。
    ///
    /// # 参数
    /// - `session_id`：被结束的会话
    /// - `end_reason`：结束原因（如 `"session_ended"` / `"engine_shutdown"`）
    ///
    /// # 错误
    /// - [`SessionError::NotFound`]：session_id 在数据库中不存在
    pub async fn end_session(
        &self,
        session_id: &str,
        end_reason: &str,
    ) -> Result<(), SessionError> {
        let mut tx = self.pool.begin().await?;

        // 校验 session 存在（与 update_title 一致，避免给不存在的 session 写脏数据）
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)")
                .bind(session_id)
                .fetch_one(&mut *tx)
                .await?;
        if !exists {
            return Err(SessionError::NotFound(session_id.to_string()));
        }

        // 秒级 f64 时间戳，与 started_at / ended_at 字段类型对齐
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        sqlx::query("UPDATE sessions SET ended_at = ?2, end_reason = ?3 WHERE id = ?1")
            .bind(session_id)
            .bind(now)
            .bind(end_reason)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        tracing::info!(
            session_id = session_id,
            end_reason = end_reason,
            "会话已标记结束"
        );
        Ok(())
    }

    /// 加载模型可见窗口消息
    ///
    /// 返回「最近一条 compaction 消息（若有）及之后的所有消息」。
    /// 从未压缩过则返回全部。核心查询（O(1) roundtrip，无递归）：
    ///
    /// ```sql
    /// SELECT * FROM messages WHERE session_id = ?
    ///   AND seq >= COALESCE(
    ///     (SELECT MAX(seq) FROM messages WHERE session_id = ? AND kind = 'compaction'),
    ///     0
    ///   )
    /// ORDER BY seq;
    /// ```
    pub async fn load_visible_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<Message>, SessionError> {
        let rows = sqlx::query_as::<_, MessageRow>(
            "SELECT id, session_id, model_id, role, content, images, tool_call_id,
                    tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                    reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind
             FROM messages
             WHERE session_id = ?1
               AND seq >= COALESCE(
                 (SELECT MAX(seq) FROM messages WHERE session_id = ?1 AND kind = 'compaction'),
                 0
               )
             ORDER BY seq",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Message::from).collect())
    }

    /// 加载全量历史（含被压缩掉的旧消息）
    ///
    /// 用途：审计、调试、导出。不参与 ReAct 循环。
    pub async fn load_full_history(&self, session_id: &str) -> Result<Vec<Message>, SessionError> {
        let rows = sqlx::query_as::<_, MessageRow>(
            "SELECT id, session_id, model_id, role, content, images, tool_call_id,
                    tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                    reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind
             FROM messages WHERE session_id = ?1 ORDER BY seq",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Message::from).collect())
    }
}
