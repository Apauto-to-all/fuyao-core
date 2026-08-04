//! 上下文压缩专用持久化（mark_compaction / load_visible_messages / load_full_history）
//!
//! 设计要点：
//! - session_id 永不变，压缩不创建新会话
//! - 压缩 = 插入一条 `kind='compaction'` 边界消息 + 更新 sessions 元数据，不复制 keep_recent
//! - 模型可见窗口 = 动态拼接：最新摘要 + 以最新摘要为起点向前切的 keep_recent + 摘要后的新消息
//!   （见 [`SessionStore::load_visible_messages`]），向前切遇到上一条 compaction 消息即停
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

        // 插入 compaction 边界消息（role='assistant'：摘要是助手产出的对话总结，
        // 以 assistant 身份参与对话流；kind='compaction' 才是真正的类型标记）
        sqlx::query(
            "INSERT INTO messages (session_id, model_id, role, content, images, tool_call_id,
                tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind)
             VALUES (?1, NULL, 'assistant', ?2, NULL, NULL, NULL, ?3, ?4, 0, 0, 0, 0, 0, NULL, NULL, ?5, 'compaction')",
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

    /// 加载模型可见窗口消息（动态拼接）
    ///
    /// # 参数
    ///
    /// - `session_id`：会话 ID
    /// - `keep_tokens`：**仅对已压缩过的会话生效**。压缩后，摘要覆盖了旧消息的全部细节，
    ///   但近期上下文（具体代码、错误原文、工具返回）必须保真给 LLM，不能只靠摘要。
    ///   `keep_tokens` 控制「附带多少条压缩前的近期消息」，单位 token，从最新摘要向前
    ///   累加至该值即停。由调用方按模型上下文比例算好传入
    ///   （`CompressionConfig::effective_keep_tokens(context_length)`），内部会强制
    ///   截断到 `keep_tokens_max` 上限——任何调用方都不可能突破这个保护。
    ///   传 0 表示「不附带压缩前的近期消息」，可见窗口只有 [摘要 + 新消息]。
    ///
    /// # 返回的可见窗口
    ///
    /// **已压缩过的会话**：三段显式拼接（顺序固定，不可依赖单一 `ORDER BY seq`，
    /// 因为摘要 seq 最大但逻辑上排最前）：
    ///
    /// ```text
    /// 可见窗口 = [最新摘要]
    ///          + select_recent( seq 在 (上一条摘要, 最新摘要) 区间的消息, keep_tokens )   ← keep_recent
    ///          + [seq > 最新摘要 的所有新消息]
    /// ```
    ///
    /// - **最新摘要**：最近一条 `kind='compaction'` 消息，是可见窗口的逻辑起点
    /// - **keep_recent**：最新摘要之前的近期消息，按 `keep_tokens` 从尾部向前保留。
    ///   只在「最近两条摘要之间」取——向前遇到上一条摘要即停，不捞回已被更早摘要覆盖的旧消息。
    ///   用原始记录（seq 不变），不复制副本
    /// - **新消息**：最新摘要之后追加的消息
    ///
    /// **从未压缩过的会话**（无任何 compaction 消息）：`keep_tokens` 不参与，直接返回全部消息。
    /// 没压缩过就没有「压缩前的消息」这个概念，不需要控制近期消息量。
    ///
    /// # 性能
    ///
    /// 几次索引查询（走 `idx_messages_session_kind_seq` / `idx_messages_session_seq`）
    /// + 内存拼接，复杂度低；仅在构造 LLM 请求时调一次，不在流式热路径。
    pub async fn load_visible_messages(
        &self,
        session_id: &str,
        keep_tokens: usize,
    ) -> Result<Vec<Message>, SessionError> {
        // keep_tokens 兜底保护：强制截断到 keep_tokens_max，任何调用方都不可能突破上限
        let keep_tokens =
            keep_tokens.min(fuyao_api::get_config().session.compression.keep_tokens_max);

        // 1. 取全部 compaction 消息的 seq（升序），用于定位最新摘要 M 与上一条摘要 M2
        let compaction_seqs: Vec<i64> = sqlx::query_scalar(
            "SELECT seq FROM messages WHERE session_id = ?1 AND kind = 'compaction' ORDER BY seq",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        // 从未压缩过：keep_tokens 不参与（没压缩就没有「压缩前的消息」需要控制量），
        // 直接返回全部消息
        if compaction_seqs.is_empty() {
            let all: Vec<Message> = sqlx::query_as::<_, MessageRow>(
                "SELECT id, session_id, model_id, role, content, images, tool_call_id,
                        tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                        reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind
                 FROM messages WHERE session_id = ?1 ORDER BY seq",
            )
            .bind(session_id)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(Message::from)
            .collect();
            return Ok(all);
        }

        // 最新摘要 M = compaction_seqs 最后一个；上一条摘要 M2 = 倒数第二个（无则视为 0）
        let latest_seq = *compaction_seqs.last().expect("非空");
        let prev_seq = if compaction_seqs.len() >= 2 {
            compaction_seqs[compaction_seqs.len() - 2]
        } else {
            0
        };

        // 2. 取最新摘要消息本身
        let latest_summary: Vec<Message> = sqlx::query_as::<_, MessageRow>(
            "SELECT id, session_id, model_id, role, content, images, tool_call_id,
                    tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                    reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind
             FROM messages
             WHERE session_id = ?1 AND seq = ?2",
        )
        .bind(session_id)
        .bind(latest_seq)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Message::from)
        .collect();

        // 3. 取 (M2, M) 区间的消息作 keep_recent 候选，按 seq 升序
        let keep_candidates: Vec<Message> = sqlx::query_as::<_, MessageRow>(
            "SELECT id, session_id, model_id, role, content, images, tool_call_id,
                    tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                    reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind
             FROM messages
             WHERE session_id = ?1 AND seq > ?2 AND seq < ?3
             ORDER BY seq",
        )
        .bind(session_id)
        .bind(prev_seq)
        .bind(latest_seq)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Message::from)
        .collect();

        // 向前切 keep_recent（基于 token 预算），用原始记录不复制
        let window = crate::compressor::window::select_recent(&keep_candidates, keep_tokens);

        // 4. 取最新摘要之后的新消息（seq > M），按 seq 升序
        let newer: Vec<Message> = sqlx::query_as::<_, MessageRow>(
            "SELECT id, session_id, model_id, role, content, images, tool_call_id,
                    tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                    reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind
             FROM messages
             WHERE session_id = ?1 AND seq > ?2
             ORDER BY seq",
        )
        .bind(session_id)
        .bind(latest_seq)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Message::from)
        .collect();

        // 5. 显式拼接：摘要（最前）+ keep_recent + 新消息
        //    摘要 seq 最大但逻辑排最前，不能靠单一 ORDER BY seq
        let mut result =
            Vec::with_capacity(latest_summary.len() + window.keep_recent.len() + newer.len());
        result.extend(latest_summary);
        result.extend(window.keep_recent.iter().cloned());
        result.extend(newer);
        Ok(result)
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

#[cfg(test)]
mod tests {
    use super::super::SessionStore;
    use super::*;
    use fuyao_api::{Message, MessageKind, MessageRole};
    use tempfile::tempdir;

    async fn temp_store() -> SessionStore {
        let dir = tempdir().expect("创建临时目录失败");
        let db_path = dir.path().join("test.db");
        std::mem::forget(dir);
        SessionStore::new(db_path).await.expect("创建存储失败")
    }

    /// 插入一条 user 消息，返回分配的 seq
    async fn insert_user(store: &SessionStore, sid: &str, content: &str) {
        let mut msg = Message::user(content.to_string());
        store.insert_message(sid, &mut msg).await.unwrap();
    }

    /// 从未压缩过：keep_tokens 足够大时返回全部，保持 seq 升序
    #[tokio::test]
    async fn load_visible_returns_all_when_never_compacted_and_budget_large() {
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();
        insert_user(&store, &session.id, "m1").await;
        insert_user(&store, &session.id, "m2").await;

        let visible = store
            .load_visible_messages(&session.id, usize::MAX)
            .await
            .unwrap();
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].content.as_deref(), Some("m1"));
        assert_eq!(visible[1].content.as_deref(), Some("m2"));
    }

    /// 单次压缩后：可见窗口 = [摘要] + keep_recent（区间内原始记录）+ 新消息
    /// keep_tokens=0 表示不保留任何 keep_recent（区间内全截断），只看摘要 + 新消息
    #[tokio::test]
    async fn load_visible_assembles_summary_keep_recent_and_newer() {
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();
        // seq 1..=3：将被压缩的旧消息
        for i in 1..=3 {
            insert_user(&store, &session.id, &format!("old{i}")).await;
        }
        // 压缩：插入 seq=4 的 compaction 边界
        store
            .mark_compaction(&session.id, "摘要1".to_string(), CompressionReason::Auto)
            .await
            .unwrap();
        // seq 5..=6：压缩后的新消息
        insert_user(&store, &session.id, "new1").await;
        insert_user(&store, &session.id, "new2").await;

        // keep_tokens 足够大：摘要 + 无 keep_recent（区间 (0,4) 内是 old1..3，
        //   但它们全部会被保留成 keep_recent）+ 新消息
        // 区间 (prev=0, latest=4) = old1/old2/old3 → 全部保留为 keep_recent
        let visible = store
            .load_visible_messages(&session.id, usize::MAX)
            .await
            .unwrap();
        // [摘要1, old1, old2, old3, new1, new2]
        assert_eq!(visible.len(), 6);
        // 摘要排最前（seq=4 最大但逻辑首位），role=assistant（助手产出的对话总结）
        assert_eq!(visible[0].kind, MessageKind::Compaction);
        assert_eq!(visible[0].role, MessageRole::Assistant);
        assert_eq!(visible[0].content.as_deref(), Some("摘要1"));
        assert_eq!(visible[0].seq, 4);
        // keep_recent 是 old1..3（seq 1..3 升序）
        assert_eq!(visible[1].content.as_deref(), Some("old1"));
        assert_eq!(visible[3].content.as_deref(), Some("old3"));
        // 新消息 seq 升序
        assert_eq!(visible[4].content.as_deref(), Some("new1"));
        assert_eq!(visible[5].content.as_deref(), Some("new2"));
    }

    /// keep_tokens=0：不附带任何压缩前的近期消息，可见窗口 = [摘要] + 新消息
    #[tokio::test]
    async fn load_visible_omits_keep_recent_when_budget_zero() {
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();
        for i in 1..=3 {
            insert_user(&store, &session.id, &format!("old{i}")).await;
        }
        store
            .mark_compaction(&session.id, "摘要1".to_string(), CompressionReason::Auto)
            .await
            .unwrap();
        insert_user(&store, &session.id, "new1").await;

        let visible = store.load_visible_messages(&session.id, 0).await.unwrap();
        // keep_tokens=0 → keep_recent 为空：可见窗口只有 [摘要, new1]
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].kind, MessageKind::Compaction);
        assert_eq!(visible[1].content.as_deref(), Some("new1"));
    }

    /// 多次压缩：向前切只在最近两条摘要之间，不捞回已被更早摘要覆盖的旧消息
    #[tokio::test]
    async fn load_visible_bounds_keep_recent_between_last_two_summaries() {
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();
        // seq 1..=2：第一批旧消息（将被摘要1 覆盖）
        insert_user(&store, &session.id, "old_a1").await;
        insert_user(&store, &session.id, "old_a2").await;
        // 摘要1：seq=3
        store
            .mark_compaction(&session.id, "摘要1".to_string(), CompressionReason::Auto)
            .await
            .unwrap();
        // seq 4..=5：摘要1 后的新消息（在两次摘要之间，应能作 keep_recent）
        insert_user(&store, &session.id, "mid1").await;
        insert_user(&store, &session.id, "mid2").await;
        // 摘要2：seq=6（最新摘要）
        store
            .mark_compaction(&session.id, "摘要2".to_string(), CompressionReason::Auto)
            .await
            .unwrap();
        // seq 7：摘要2 后的新消息
        insert_user(&store, &session.id, "new1").await;

        // keep_tokens 足够大：可见窗口 = [摘要2] + keep_recent(区间(3,6)=mid1/mid2) + [new1]
        // 关键：old_a1/old_a2（seq 1,2）在摘要1（seq=3）之前，已被覆盖，绝不被捞回
        let visible = store
            .load_visible_messages(&session.id, usize::MAX)
            .await
            .unwrap();
        assert_eq!(visible.len(), 4, "应只含 摘要2 + mid1 + mid2 + new1");
        assert_eq!(visible[0].kind, MessageKind::Compaction);
        assert_eq!(visible[0].content.as_deref(), Some("摘要2"));
        // keep_recent 是摘要1 与摘要2 之间的 mid1/mid2，不含 old_a*
        let contents: Vec<_> = visible.iter().filter_map(|m| m.content.clone()).collect();
        assert!(contents.contains(&"mid1".to_string()));
        assert!(contents.contains(&"mid2".to_string()));
        assert!(
            !contents.contains(&"old_a1".to_string()),
            "已被摘要1覆盖的旧消息不应被捞回"
        );
        assert!(!contents.contains(&"old_a2".to_string()));
        assert_eq!(visible[3].content.as_deref(), Some("new1"));
    }
}
