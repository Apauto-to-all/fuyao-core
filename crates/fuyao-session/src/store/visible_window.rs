//! LLM 可见窗口查询
//!
//! [`SessionStore::load_visible_messages`] 是给 LLM 构造 `ChatRequest` 用的「可见窗口」查询:
//! 压缩感知、尊重压缩边界。与给人看的历史浏览(`message::load_full_history` /
//! `message::list_messages_before`)是不同口径的查询路径,互不影响。
//!
//! # 窗口语义
//!
//! - **已压缩过的会话**:可见窗口 = [最新 compaction 摘要] + [seq 大于该摘要的全部新消息]。
//!   摘要覆盖其全部先前消息,旧消息对 LLM 不可见
//! - **从未压缩过的会话**(无任何 compaction 消息):返回全部消息

use super::row::MessageRow;
use crate::error::SessionError;
use fuyao_api::Message;
use sqlx::AssertSqlSafe;

impl super::SessionStore {
    /// 加载模型可见窗口消息(压缩感知)
    ///
    /// 这是给 LLM 构造 ChatRequest 用的「可见窗口」查询:尊重压缩边界。
    /// 与给人看的历史浏览(`load_full_history` / `list_messages_before`)
    /// 是不同口径的查询路径。
    ///
    /// # 参数
    ///
    /// - `session_id`:会话 ID
    ///
    /// # 返回的可见窗口
    ///
    /// - **已压缩过的会话**:[最新 compaction 摘要] + [seq 大于该摘要的全部新消息]。
    ///   最新摘要是可见窗口的逻辑起点,其全部先前消息已被覆盖,对 LLM 不暴露
    /// - **从未压缩过的会话**(无任何 compaction 消息):返回全部消息
    ///
    /// # 性能
    ///
    /// 单条 SQL(标量子查询定位最新 compaction 摘要的 seq,主查询走
    /// `UNIQUE(session_id, seq)` 约束自带的隐式索引;从未压缩是 COALESCE
    /// 退化成全量的自然特例),无 Rust 侧拼接切分;仅在构造 LLM 请求时调一次,
    /// 不在流式热路径。
    pub async fn load_visible_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<Message>, SessionError> {
        // 单边界查询:窗口下界取共享可见窗口谓词(seq >= 最新摘要的 seq,无摘要则
        // COALESCE 退化成 0,即全量),与 fork_visible 的整窗复制物理同源。
        // 摘要行本身的 seq 在结果集内最小、ORDER BY seq 后排最前,天然有序。
        let rows: Vec<MessageRow> = sqlx::query_as::<_, MessageRow>(AssertSqlSafe(format!(
            "SELECT id, session_id, model_id, role, content, images, tool_call_id,
                    tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                    reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind
            FROM messages
            WHERE session_id = ?1 {}
            ORDER BY seq",
            super::sql::VISIBLE_WINDOW_PREDICATE
        )))
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Message::from).collect())
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

    // ===== load_visible_messages 测试(可见窗口查询) =====

    #[tokio::test]
    async fn load_visible_returns_all_when_never_compacted() {
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();
        insert_user(&store, &session.id, "m1").await;
        insert_user(&store, &session.id, "m2").await;

        let visible = store.load_visible_messages(&session.id).await.unwrap();
        assert_eq!(visible.len(), 2);
    }

    #[tokio::test]
    async fn load_visible_exposes_summary_and_newer_after_boundary() {
        // 压缩后:可见窗口 = [最新摘要] + 摘要后新消息,摘要前的旧消息不可见
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();

        for content in ["old1", "old2", "old3"] {
            insert_user(&store, &session.id, content).await;
        }
        // 标记压缩:此时 seq=4 是 compaction 边界
        store
            .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
            .await
            .unwrap();
        insert_user(&store, &session.id, "new1").await;

        let visible = store.load_visible_messages(&session.id).await.unwrap();
        // [摘要, new1]:摘要排最前,old1~old3 在摘要之前已被覆盖,新消息在末尾
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].kind, MessageKind::Compaction);
        assert_eq!(visible[0].seq, 4);
        assert_eq!(visible[1].content.as_deref(), Some("new1"));
        assert_eq!(visible[1].seq, 5);
    }

    #[tokio::test]
    async fn load_visible_starts_from_latest_summary_only() {
        // 多次压缩:可见窗口只从最新摘要起,两次摘要之间的消息也已被最新摘要覆盖
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();

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

        let visible = store.load_visible_messages(&session.id).await.unwrap();
        // [摘要2]:v2-1 在摘要2 之前,已被覆盖,不可见
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].kind, MessageKind::Compaction);
        assert_eq!(visible[0].content.as_deref(), Some("摘要2"));
    }
}
