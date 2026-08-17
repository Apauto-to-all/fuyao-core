//! LLM 可见窗口的压缩感知动态拼接
//!
//! [`SessionStore::load_visible_messages`] 是给 LLM 构造 `ChatRequest` 用的「可见窗口」查询:
//! 控制 token、尊重压缩边界、保留近期上下文。与给人看的历史浏览(`message::load_full_history`
//! / `message::list_messages_before`)是不同口径的查询路径,互不影响。
//!
//! # 拼接规则
//!
//! 已压缩过的会话,可见窗口三段显式拼接(顺序固定,不依赖单一 `ORDER BY seq`,
//! 因为摘要 seq 最大但逻辑上排最前):
//!
//! ```text
//! 可见窗口 = [最新摘要]
//!          + select_recent( seq 在 (上一条摘要, 最新摘要) 区间的消息, keep_tokens )   ← keep_recent
//!          + [seq > 最新摘要 的所有新消息]
//! ```
//!
//! 多次压缩正确性:每条 `kind='compaction'` 消息都是天然的不可逾越边界——
//! 向前切遇到上一条摘要即停,不捞回已被更早摘要覆盖的旧消息。

use super::row::MessageRow;
use super::window::select_recent;
use crate::error::SessionError;
use fuyao_api::Message;

impl super::SessionStore {
    /// 加载模型可见窗口消息(动态拼接,压缩感知)
    ///
    /// 这是给 LLM 构造 ChatRequest 用的「可见窗口」查询:控制 token、尊重压缩边界、
    /// 保留近期上下文。与给人看的历史浏览(`load_full_history` / `list_messages_before`)
    /// 是不同口径的查询路径。
    ///
    /// # 参数
    ///
    /// - `session_id`:会话 ID
    /// - `keep_tokens`:**仅对已压缩过的会话生效**。压缩后,摘要覆盖了旧消息的全部细节,
    ///   但近期上下文(具体代码、错误原文、工具返回)必须保真给 LLM,不能只靠摘要。
    ///   `keep_tokens` 控制「附带多少条压缩前的近期消息」,单位 token,从最新摘要向前
    ///   累加至该值即停。由调用方按模型上下文比例算好传入
    ///   (`CompressionConfig::effective_keep_tokens(context_length)`),内部会强制
    ///   截断到 `keep_tokens_max` 上限——任何调用方都不可能突破这个保护。
    ///   传 0 表示「不附带压缩前的近期消息」,可见窗口只有 [摘要 + 新消息]。
    ///
    /// # 返回的可见窗口
    ///
    /// **已压缩过的会话**:三段显式拼接(顺序固定,不可依赖单一 `ORDER BY seq`,
    /// 因为摘要 seq 最大但逻辑上排最前):
    ///
    /// ```text
    /// 可见窗口 = [最新摘要]
    ///          + select_recent( seq 在 (上一条摘要, 最新摘要) 区间的消息, keep_tokens )   ← keep_recent
    ///          + [seq > 最新摘要 的所有新消息]
    /// ```
    ///
    /// - **最新摘要**:最近一条 `kind='compaction'` 消息,是可见窗口的逻辑起点
    /// - **keep_recent**:最新摘要之前的近期消息,按 `keep_tokens` 从尾部向前保留。
    ///   只在「最近两条摘要之间」取——向前遇到上一条摘要即停,不捞回已被更早摘要覆盖的旧消息。
    ///   用原始记录(seq 不变),不复制副本
    /// - **新消息**:最新摘要之后追加的消息
    ///
    /// **从未压缩过的会话**(无任何 compaction 消息):`keep_tokens` 不参与,直接返回全部消息。
    /// 没压缩过就没有「压缩前的消息」这个概念,不需要控制近期消息量。
    ///
    /// # 性能
    ///
    /// 几次索引查询(走 `idx_messages_session_kind_seq` /
    /// `UNIQUE(session_id, seq)` 约束自带的隐式索引)
    /// + 内存拼接,复杂度低;仅在构造 LLM 请求时调一次,不在流式热路径。
    pub async fn load_visible_messages(
        &self,
        session_id: &str,
        keep_tokens: usize,
    ) -> Result<Vec<Message>, SessionError> {
        // keep_tokens 兜底保护:强制截断到 keep_tokens_max,任何调用方都不可能突破上限
        let keep_tokens =
            keep_tokens.min(fuyao_api::get_config().session.compression.keep_tokens_max);

        // 1. 取全部 compaction 消息的 seq(升序),用于定位最新摘要 M 与上一条摘要 M2
        let compaction_seqs: Vec<i64> = sqlx::query_scalar(
            "SELECT seq FROM messages WHERE session_id = ?1 AND kind = 'compaction' ORDER BY seq",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        // 从未压缩过:keep_tokens 不参与(没压缩就没有「压缩前的消息」需要控制量),
        // 直接返回全部消息
        if compaction_seqs.is_empty() {
            return self.load_full_history(session_id).await;
        }

        // 最新摘要 M = compaction_seqs 最后一个;上一条摘要 M2 = 倒数第二个(无则视为 0)
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

        // 3. 取 (M2, M) 区间的消息作 keep_recent 候选,按 seq 升序
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

        // 向前切 keep_recent(基于 token 预算),用原始记录不复制
        let window = select_recent(&keep_candidates, keep_tokens);

        // 4. 取最新摘要之后的新消息(seq > M),按 seq 升序
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

        // 5. 显式拼接:摘要(最前)+ keep_recent + 新消息
        //    摘要 seq 最大但逻辑排最前,不能靠单一 ORDER BY seq
        let mut result =
            Vec::with_capacity(latest_summary.len() + window.keep_recent.len() + newer.len());
        result.extend(latest_summary);
        result.extend(window.keep_recent.iter().cloned());
        result.extend(newer);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::super::SessionStore;
    use crate::store::compaction::CompressionReason;
    use fuyao_api::{Message, MessageKind};

    /// 构造临时存储(隔离的临时目录)
    async fn temp_store() -> SessionStore {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let db_path = dir.path().join("test.db");
        std::mem::forget(dir);
        SessionStore::new(db_path).await.expect("创建存储失败")
    }

    /// 插入一条 user 消息
    async fn insert_user(store: &SessionStore, sid: &str, content: &str) {
        let mut msg = Message::user(content.to_string());
        store.insert_message(sid, &mut msg).await.unwrap();
    }

    // ===== load_visible_messages 测试(可见窗口动态拼接) =====

    #[tokio::test]
    async fn load_visible_returns_all_when_never_compacted() {
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
    }

    #[tokio::test]
    async fn load_visible_assembles_dynamic_window_after_boundary() {
        // 动态拼接:可见窗口 = [最新摘要] + keep_recent(区间内原始记录) + 摘要后新消息
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        for content in ["old1", "old2", "old3"] {
            insert_user(&store, &session.id, content).await;
        }
        // 标记压缩:此时 seq=4 是 compaction 边界
        store
            .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
            .await
            .unwrap();
        insert_user(&store, &session.id, "new1").await;

        let visible = store
            .load_visible_messages(&session.id, usize::MAX)
            .await
            .unwrap();
        // [摘要, old1, old2, old3, new1]:摘要排最前,keep_recent 含区间内原始记录,新消息在末尾
        assert_eq!(visible.len(), 5);
        assert_eq!(visible[0].kind, MessageKind::Compaction);
        assert_eq!(visible[0].seq, 4);
        assert_eq!(visible[1].content.as_deref(), Some("old1"));
        assert_eq!(visible[3].content.as_deref(), Some("old3"));
        assert_eq!(visible[4].content.as_deref(), Some("new1"));
        assert_eq!(visible[4].seq, 5);
    }

    #[tokio::test]
    async fn load_visible_omits_keep_recent_when_budget_zero() {
        // keep_tokens=0:不附带任何压缩前近期消息,可见窗口 = [摘要] + 新消息
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();
        for content in ["old1", "old2", "old3"] {
            insert_user(&store, &session.id, content).await;
        }
        store
            .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
            .await
            .unwrap();
        insert_user(&store, &session.id, "new1").await;

        let visible = store.load_visible_messages(&session.id, 0).await.unwrap();
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].kind, MessageKind::Compaction);
        assert_eq!(visible[1].content.as_deref(), Some("new1"));
    }

    #[tokio::test]
    async fn load_visible_keeps_window_between_last_two_summaries() {
        // 多次压缩:keep_recent 只取最近两条摘要之间,不捞回已被更早摘要覆盖的旧消息
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        insert_user(&store, &session.id, "v1-1").await;
        let seq1 = store
            .mark_compaction(&session.id, "摘要1".to_string(), CompressionReason::Auto)
            .await
            .unwrap();
        insert_user(&store, &session.id, "v2-1").await;
        let seq2 = store
            .mark_compaction(&session.id, "摘要2".to_string(), CompressionReason::Auto)
            .await
            .unwrap();
        assert!(seq2 > seq1);

        let visible = store
            .load_visible_messages(&session.id, usize::MAX)
            .await
            .unwrap();
        // [摘要2, v2-1]:摘要2(最近一次边界)排最前,v2-1 是两次摘要之间的 keep_recent
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].kind, MessageKind::Compaction);
        assert_eq!(visible[0].content.as_deref(), Some("摘要2"));
        assert_eq!(visible[1].content.as_deref(), Some("v2-1"));
    }
}
