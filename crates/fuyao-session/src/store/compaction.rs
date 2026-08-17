//! 压缩边界写入
//!
//! 上下文压缩的持久化落地:把一次压缩产出的摘要写成一条 `kind='compaction'` 边界消息,
//! 并更新 sessions 表的压缩元数据。这是压缩动作在 DB 侧的唯一写入点。
//!
//! # 设计要点
//!
//! - session_id 永不变,压缩不创建新会话
//! - 压缩 = 插入一条 `kind='compaction'` 边界消息 + 更新 sessions 元数据,不复制 keep_recent
//! - 旧消息物理保留(不删除、不归档),可审计可恢复
//! - 统计字段(token 累计)压缩不重置
//!
//! 可见窗口的读取(给 LLM 构造请求用的动态拼接)见 [`super::message::SessionStore::load_visible_messages`]。
//! 给人看的全量历史见 [`super::message::SessionStore::load_full_history`]。

use crate::error::SessionError;

/// 压缩触发原因(写入 compaction 消息时附带,便于审计)
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

/// 从事件层枚举转换：事件层 `fuyao_api::message::output::CompressionReason` 与本持久层枚举同构，
/// 由消费方（fuyao-core）在落库时 `.into()` 转换，保证 DB 审计列与 Started/Ended 事件 reason 一致
impl From<fuyao_api::message::output::CompressionReason> for CompressionReason {
    fn from(reason: fuyao_api::message::output::CompressionReason) -> Self {
        match reason {
            fuyao_api::message::output::CompressionReason::Auto => Self::Auto,
            fuyao_api::message::output::CompressionReason::Manual => Self::Manual,
            fuyao_api::message::output::CompressionReason::Overflow => Self::Overflow,
        }
    }
}

impl super::SessionStore {
    /// 标记一次压缩完成
    ///
    /// 事务内做三件事:
    /// 1. 给 messages 表插入一条 `kind='compaction'` 的边界消息(content=summary,
    ///    seq 分配内联在 INSERT 的标量子查询中)
    /// 2. 更新 sessions.`last_compacted_seq` = 新 compaction 消息的 seq
    /// 3. sessions.`compression_count` += 1
    ///
    /// 不创建新 session、不动 session_id、不重置统计字段、不动 end_reason/ended_at。
    ///
    /// # 参数
    /// - `session_id`:被压缩的会话(永不变)
    /// - `summary`:摘要正文(Markdown),存入 compaction 消息的 content
    /// - `reason`:压缩触发原因
    ///
    /// # 返回
    /// 新 compaction 消息的 seq(同时也成为 `last_compacted_seq`)
    ///
    /// # 错误
    /// - [`SessionError::NotFound`]:session_id 在数据库中不存在
    pub async fn mark_compaction(
        &self,
        session_id: &str,
        summary: String,
        reason: CompressionReason,
    ) -> Result<i64, SessionError> {
        let mut tx = self.pool.begin().await?;

        // 验证 session 存在(避免给不存在的 session 插孤儿消息)
        super::session::require_session(&mut tx, session_id).await?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        // 插入 compaction 边界消息(role='assistant':摘要是助手产出的对话总结,
        // 以 assistant 身份参与对话流;kind='compaction' 才是真正的类型标记)。
        // seq 分配折叠进 INSERT 的 VALUES 槽位(标量子查询 COALESCE(MAX(seq), 0) + 1,
        // 事务内保证并发安全),RETURNING 取回实际分配值,省掉一次独立的 MAX(seq)
        // 预查询往返。
        let (next_seq,): (i64,) = sqlx::query_as(
            "INSERT INTO messages (session_id, model_id, role, content, images, tool_call_id,
                tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind)
             VALUES (?1, NULL, 'assistant', ?2, NULL, NULL, NULL, ?3, ?4, 0, 0, 0, 0, 0, NULL, NULL,
                (SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE session_id = ?1), 'compaction')
             RETURNING seq",
        )
        .bind(session_id)
        .bind(&summary)
        .bind(reason.as_str())
        .bind(now)
        .fetch_one(&mut *tx)
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
}

#[cfg(test)]
mod tests {
    use super::super::SessionStore;
    use super::*;
    use crate::error::SessionError;
    use fuyao_api::{Message, MessageKind, MessageRole};

    /// 构造临时存储(隔离的临时目录)
    async fn temp_store() -> SessionStore {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let db_path = dir.path().join("test.db");
        std::mem::forget(dir);
        SessionStore::new(db_path).await.expect("创建存储失败")
    }

    // ===== mark_compaction 测试 =====

    #[tokio::test]
    async fn mark_compaction_inserts_boundary_message() {
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        let mut m1 = Message::user("hello".to_string());
        store.insert_message(&session.id, &mut m1).await.unwrap();
        let mut m2 = Message::assistant(Some("hi".to_string()));
        store.insert_message(&session.id, &mut m2).await.unwrap();

        let new_seq = store
            .mark_compaction(
                &session.id,
                "## 目标\n- 测试".to_string(),
                CompressionReason::Auto,
            )
            .await
            .unwrap();

        // 新 seq = 3(前面有 2 条消息)
        assert_eq!(new_seq, 3);

        // 全量历史能看到这条边界
        let full = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(full.len(), 3);
        let boundary = &full[2];
        assert_eq!(boundary.seq, 3);
        assert_eq!(boundary.kind, MessageKind::Compaction);
        assert_eq!(boundary.role, MessageRole::Assistant);
        assert_eq!(boundary.content.as_deref(), Some("## 目标\n- 测试"));
    }

    #[tokio::test]
    async fn mark_compaction_updates_session_metadata() {
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        let new_seq = store
            .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
            .await
            .unwrap();

        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.last_compacted_seq, Some(new_seq));
        assert_eq!(loaded.compression_count, 1);
        assert_eq!(loaded.total_prompt_tokens, 0);
        assert!(loaded.ended_at.is_none());
        assert!(loaded.end_reason.is_none());
    }

    #[tokio::test]
    async fn mark_compaction_returns_not_found_for_missing_session() {
        let store = temp_store().await;
        let result = store
            .mark_compaction("nonexistent", "摘要".to_string(), CompressionReason::Auto)
            .await;
        assert!(matches!(result, Err(SessionError::NotFound(_))));
    }

    // ===== fork:跨 session+compaction 的数据复制场景 =====

    #[tokio::test]
    async fn fork_copy_visible_messages_to_child() {
        // 模拟 Engine 的 fork 拷贝数据层机制:
        // 源 session 有可见消息 → 新建子 session → 逐条 insert_message 复制 →
        // 子 session 的可见窗口应与源一致(含 compaction 边界,若有)
        let store = temp_store().await;

        // 源 session:2 条普通消息
        let parent = fuyao_api::Session::new(None, None, Some("父系统提示词".to_string()));
        store.create(&parent).await.unwrap();
        let mut m1 = Message::user("父消息1".to_string());
        store.insert_message(&parent.id, &mut m1).await.unwrap();
        let mut m2 = Message::assistant(Some("父回复".to_string()));
        store.insert_message(&parent.id, &mut m2).await.unwrap();

        // 加一次压缩,让源 session 的可见窗口含 compaction 边界
        store
            .mark_compaction(&parent.id, "父摘要".to_string(), CompressionReason::Auto)
            .await
            .unwrap();
        let mut m3 = Message::user("压缩后消息".to_string());
        store.insert_message(&parent.id, &mut m3).await.unwrap();

        let source_visible = store
            .load_visible_messages(&parent.id, usize::MAX)
            .await
            .unwrap();
        // 源可见 = 动态拼接窗口:[compaction 边界, 父消息1, 父回复, 压缩后消息]
        assert_eq!(source_visible.len(), 4);

        // 构造子 session(复制源系统提示词 + 标记 parent_session_id)
        // message_count 不预设:从 0 起算,下方逐条 insert_message 复制消息时,
        // 事务内会按 kind 原子累加(普通消息 +1,compaction 边界不计),复制完成后
        // DB 里的 message_count 自然对齐复制的普通消息条数(新架构:DB 唯一数据源)。
        let non_compaction_count = source_visible
            .iter()
            .filter(|m| matches!(m.kind, MessageKind::Message))
            .count();
        let mut child = fuyao_api::Session::new(None, None, parent.system_prompt.clone());
        child.parent_session_id = Some(parent.id.clone());
        store.create(&child).await.unwrap();

        // 逐条复制可见消息
        for msg in &source_visible {
            let mut clone = msg.clone();
            clone.seq = 0;
            store.insert_message(&child.id, &mut clone).await.unwrap();
        }

        // 子 session 的可见窗口应与源一致
        let child_visible = store
            .load_visible_messages(&child.id, usize::MAX)
            .await
            .unwrap();
        assert_eq!(child_visible.len(), source_visible.len());
        assert_eq!(child_visible[0].kind, MessageKind::Compaction);
        assert_eq!(child_visible[0].content.as_deref(), Some("父摘要"));
        let source_contents: Vec<_> = source_visible.iter().map(|m| m.content.clone()).collect();
        let child_contents: Vec<_> = child_visible.iter().map(|m| m.content.clone()).collect();
        assert_eq!(source_contents, child_contents, "子可见窗口应与源逐条一致");

        // 子 session 自身统计从 0 起算(费用 / token 不继承源),仅 message_count 对齐复制条数
        let child_meta = store.get(&child.id).await.unwrap().unwrap();
        assert_eq!(
            child_meta.parent_session_id.as_deref(),
            Some(parent.id.as_str())
        );
        assert_eq!(child_meta.message_count, non_compaction_count as i64);
        assert_eq!(child_meta.total_prompt_tokens, 0);
        assert_eq!(child_meta.total_cost, 0.0);
        // 子 session 从未压缩过:自己的压缩指针为空(源的压缩边界只是被复制成普通行)
        assert!(child_meta.last_compacted_seq.is_none());

        assert_eq!(
            store.count_messages(&child.id).await.unwrap(),
            non_compaction_count as i64
        );
    }
}
