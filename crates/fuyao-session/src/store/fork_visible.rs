//! 可见窗口派生（fork_visible）
//!
//! [`SessionStore::fork_visible`] 把源会话的**可见窗口**整窗复制为新会话：以最新
//! compaction 摘要为逻辑起点（无摘要则全量），摘要行与其后全部消息原样进入新会话，
//! 源会话不动任何数据。
//!
//! # 与 [`fork`](super::fork) 的分工
//!
//! - [`SessionStore::fork_to`]：按目标点截断分支（目标须为 user / compaction 消息，
//!   复制 `seq < target`），产出独立主会话（parent 恒 None）——服务「分支对话」
//! - [`SessionStore::fork_visible`]：无目标点概念，整窗复制可见上下文，
//!   `parent_session_id` 由调用方指定——服务「子任务继承上下文」（引擎子任务
//!   session 的 Fork 模式）
//!
//! # 复制语义
//!
//! - **新 session**：workspace 与 system_prompt 复制源持久化原值（源提示词可能已被
//!   压缩重建过，重建值才是模型看到的）；标题取默认；count 类与消费类从 0 起算、
//!   按复制结果聚合；压缩元数据不继承（新会话自身从未执行压缩，复制进来的
//!   compaction 边界行本身就是可见窗口下界）
//! - **消息**：可见窗口整窗复制，seq 从 1 起连续重新分配
//!
//! # 单事务原子
//!
//! 校验 + 建行 + 复制 + 重算在一个 SQLite 事务内完成，中途失败整体回滚，
//! 不留半成品会话。
//!
//! # 与运行态的关系
//!
//! 只读源会话、只写新会话，与源会话的活跃 turn 互不干扰。

use super::row::MessageRow;
use crate::error::SessionError;
use fuyao_api::{Message, MessageKind, MessageRole, Session};

impl super::SessionStore {
    /// 把源会话的可见窗口整窗复制为新会话，`parent_session_id` 由调用方指定
    ///
    /// 单事务原子，四步：
    /// 1. 取源 session 行（workspace / system_prompt），不存在报 NotFound
    /// 2. 事务内加载可见窗口（最新 compaction 摘要 + 摘要后全部消息，无摘要则全量，
    ///    与 [`SessionStore::load_visible_messages`](super::visible_window::SessionStore::load_visible_messages)
    ///    同一口径）
    /// 3. 构造新 session 行落库（parent 由参数决定，随机 id 主键冲突重新生成重试）
    /// 4. 复制窗口消息（seq 连续重分配）并按复制结果聚合累加 count 类 / 消费类字段
    ///
    /// # 参数
    /// - `source_id`:源会话（本操作不动其任何数据）
    /// - `parent_session_id`:新会话的父标记（`Some` = 子任务会话，`None` = 独立主会话）
    ///
    /// # 返回
    /// 新会话 id。分支的后续状态经既有读路径获取：`load_visible_messages` /
    /// `load_full_history` 看分支消息，`get` 看分支元数据。
    ///
    /// # 错误
    /// - [`SessionError::NotFound`]:source_id 不存在
    pub async fn fork_visible(
        &self,
        source_id: &str,
        parent_session_id: Option<String>,
    ) -> Result<String, SessionError> {
        let mut tx = self.pool.begin().await?;

        // 1. 取源 session 行：新会话继承 workspace 与 system_prompt
        let source_row: Option<(Option<String>, Option<String>)> =
            sqlx::query_as("SELECT workspace, system_prompt FROM sessions WHERE id = ?1")
                .bind(source_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((workspace, system_prompt)) = source_row else {
            return Err(SessionError::NotFound(source_id.to_string()));
        };

        // 2. 事务内加载可见窗口（与 load_visible_messages 同口径的单条 SQL：
        //    seq >= 最新 compaction 摘要的 seq，无摘要 COALESCE 退化为全量）
        let rows: Vec<MessageRow> = sqlx::query_as::<_, MessageRow>(
            "SELECT id, session_id, model_id, role, content, images, tool_call_id,
                    tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                    reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind
             FROM messages
             WHERE session_id = ?1
               AND seq >= COALESCE((SELECT MAX(seq) FROM messages
                                    WHERE session_id = ?1 AND kind = 'compaction'), 0)
             ORDER BY seq",
        )
        .bind(source_id)
        .fetch_all(&mut *tx)
        .await?;
        let copied: Vec<Message> = rows.into_iter().map(Message::from).collect();

        // 3. 构造新 session 行并落库：parent 由调用方指定，计数字段从 0 起算
        //    （第 4 步按复制结果累加到位）。随机 id 主键冲突时重新生成重试，
        //    仅重试主键冲突——其它错误（磁盘满、连接断等）重试无意义且掩盖真实故障
        let mut new_session = Session::new(workspace, None, system_prompt);
        new_session.parent_session_id = parent_session_id;
        for attempt in 0..=super::session::ID_CONFLICT_MAX_RETRIES {
            match Self::insert_session_row(&mut *tx, &new_session).await {
                Ok(()) => break,
                Err(e) => {
                    if e.is_primary_key_conflict()
                        && attempt < super::session::ID_CONFLICT_MAX_RETRIES
                    {
                        tracing::warn!(
                            attempt = attempt + 1,
                            session_id = %new_session.id,
                            cause = "session id 主键冲突，重新生成 id 重试",
                        );
                        new_session.regenerate_id();
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        // 4. 复制可见窗口消息到新会话：seq 从事务内 COALESCE(MAX(seq), 0) + 1 起点连续
        //    递增分配（新会话无消息，起点即 1）
        let base_seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE session_id = ?1",
        )
        .bind(&new_session.id)
        .fetch_one(&mut *tx)
        .await?;
        for (offset, msg) in copied.iter().enumerate() {
            Self::insert_message_row(&mut tx, &new_session.id, msg, base_seq + offset as i64)
                .await?;
        }

        // 按复制结果聚合累加 count 类 / 消费类字段（新会话行从 0 起算，聚合即终值）。
        // 仅 kind='message' 的普通消息参与——compaction 边界行不增计数、
        // 不计 token/费用，也不刷新 last_active_at
        let normal: Vec<&Message> = copied
            .iter()
            .filter(|m| matches!(m.kind, MessageKind::Message))
            .collect();
        if !normal.is_empty() {
            let message_delta: i64 = normal.len() as i64;
            let tool_delta: i64 = normal
                .iter()
                .filter(|m| matches!(m.role, MessageRole::Tool))
                .count() as i64;
            let prompt_sum: i64 = normal.iter().map(|m| m.prompt_tokens).sum();
            let completion_sum: i64 = normal.iter().map(|m| m.completion_tokens).sum();
            let reasoning_sum: i64 = normal.iter().map(|m| m.reasoning_tokens).sum();
            let cached_sum: i64 = normal.iter().map(|m| m.cached_tokens).sum();
            let cost_sum: f64 = normal.iter().map(|m| m.cost).sum();

            sqlx::query(
                "UPDATE sessions SET
                    message_count = message_count + ?2,
                    tool_call_count = tool_call_count + ?3,
                    total_prompt_tokens = total_prompt_tokens + ?4,
                    total_completion_tokens = total_completion_tokens + ?5,
                    total_reasoning_tokens = total_reasoning_tokens + ?6,
                    total_cached_tokens = total_cached_tokens + ?7,
                    total_cost = total_cost + ?8,
                    last_active_at = unixepoch()
                 WHERE id = ?1",
            )
            .bind(&new_session.id)
            .bind(message_delta)
            .bind(tool_delta)
            .bind(prompt_sum)
            .bind(completion_sum)
            .bind(reasoning_sum)
            .bind(cached_sum)
            .bind(cost_sum)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

        tracing::info!(
            source_session_id = source_id,
            new_session_id = %new_session.id,
            parent_session_id = new_session.parent_session_id.as_deref().unwrap_or(""),
            message_count = normal.len() as i64,
            "可见窗口已整窗复制为新会话"
        );

        Ok(new_session.id)
    }
}

#[cfg(test)]
mod tests {
    use super::super::SessionStore;
    use crate::error::SessionError;
    use crate::store::compaction::CompressionReason;
    use fuyao_api::{Message, MessageKind};

    /// 构造临时存储（隔离的临时目录）
    async fn temp_store() -> SessionStore {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let db_path = dir.path().join("test.db");
        std::mem::forget(dir);
        SessionStore::new(db_path).await.expect("创建存储失败")
    }

    /// 插入一条 user 消息，返回其 seq
    async fn insert_user(store: &SessionStore, sid: &str, content: &str) -> i64 {
        let mut msg = Message::user(content.to_string());
        store.insert_message(sid, &mut msg).await.unwrap()
    }

    /// 插入一条 assistant 消息，返回其 seq
    async fn insert_assistant(store: &SessionStore, sid: &str, content: &str) -> i64 {
        let mut msg = Message::assistant(Some(content.to_string()));
        store.insert_message(sid, &mut msg).await.unwrap()
    }

    /// 插入一条 tool 结果消息，返回其 seq
    async fn insert_tool(
        store: &SessionStore,
        sid: &str,
        tool_call_id: &str,
        content: &str,
    ) -> i64 {
        let mut msg = Message::tool_result(
            tool_call_id.to_string(),
            "echo".to_string(),
            content.to_string(),
        );
        store.insert_message(sid, &mut msg).await.unwrap()
    }

    /// 落库一个带 workspace 与 system_prompt 的源会话
    async fn seed_source(store: &SessionStore) -> fuyao_api::Session {
        let session = fuyao_api::Session::new(
            Some("/proj".to_string()),
            None,
            Some("系统提示词".to_string()),
        );
        store.create(&session).await.unwrap();
        session
    }

    // ===== 整窗复制 + parent 标记 =====

    #[tokio::test]
    async fn fork_visible_copies_visible_window_and_sets_parent() {
        // 场景：u1, a1, [compaction@3], u2, a2 → 可见窗口 = [摘要, u2, a2]
        //       → fork_visible(parent=Some) → 新会话只含窗口三条，压缩前旧消息不进
        let store = temp_store().await;
        let session = seed_source(&store).await;

        insert_user(&store, &session.id, "u1").await; // seq 1（压缩前，不可见）
        insert_assistant(&store, &session.id, "a1").await; // seq 2（压缩前，不可见）
        store
            .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
            .await
            .unwrap(); // seq 3
        insert_user(&store, &session.id, "u2").await; // seq 4
        insert_assistant(&store, &session.id, "a2").await; // seq 5

        let new_id = store
            .fork_visible(&session.id, Some("parent-1".to_string()))
            .await
            .unwrap();

        // 新会话 parent = 指定父 id，workspace 与 system_prompt 复制源值
        let meta = store.get(&new_id).await.unwrap().unwrap();
        assert_eq!(meta.parent_session_id.as_deref(), Some("parent-1"));
        assert_eq!(meta.workspace.as_deref(), Some("/proj"));
        assert_eq!(meta.system_prompt.as_deref(), Some("系统提示词"));

        // 窗口整窗复制：摘要行（kind=compaction）+ 摘要后两条，seq 从 1 连续重分配
        let history = store.load_full_history(&new_id).await.unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].kind, MessageKind::Compaction);
        assert_eq!(history[0].content.as_deref(), Some("摘要"));
        assert_eq!(history[0].seq, 1);
        assert_eq!(history[1].content.as_deref(), Some("u2"));
        assert_eq!(history[1].seq, 2);
        assert_eq!(history[2].content.as_deref(), Some("a2"));
        assert_eq!(history[2].seq, 3);

        // count 类只数普通消息（摘要行不计）；压缩元数据不继承
        assert_eq!(meta.message_count, 2);
        assert_eq!(meta.compression_count, 0);
        assert!(meta.last_compacted_seq.is_none());

        // 源会话原样不动：5 条消息
        let source = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(source.len(), 5);
    }

    // ===== 从未压缩的源：全量复制 =====

    #[tokio::test]
    async fn fork_visible_without_compaction_copies_all_messages() {
        let store = temp_store().await;
        let session = seed_source(&store).await;

        insert_user(&store, &session.id, "u1").await;
        insert_assistant(&store, &session.id, "a1").await;

        let new_id = store
            .fork_visible(&session.id, Some("parent-2".to_string()))
            .await
            .unwrap();

        let history = store.load_full_history(&new_id).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content.as_deref(), Some("u1"));
        assert_eq!(history[1].content.as_deref(), Some("a1"));
    }

    // ===== count 类与消费类按复制结果聚合 =====

    #[tokio::test]
    async fn fork_visible_aggregates_counts_and_consumption() {
        // assistant 消息携带的 token/cost 求和进新会话；tool 结果计入 tool_call_count
        let store = temp_store().await;
        let session = seed_source(&store).await;

        insert_user(&store, &session.id, "u1").await; // seq 1
        let mut a1 = Message::assistant(None);
        a1.prompt_tokens = 1000;
        a1.completion_tokens = 200;
        a1.cost = 0.03;
        a1.tool_calls = Some(vec![fuyao_api::ToolCallData {
            id: "call_1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        }]);
        store.insert_message(&session.id, &mut a1).await.unwrap(); // seq 2
        insert_tool(&store, &session.id, "call_1", "结果1").await; // seq 3

        let new_id = store
            .fork_visible(&session.id, Some("parent-3".to_string()))
            .await
            .unwrap();

        let meta = store.get(&new_id).await.unwrap().unwrap();
        assert_eq!(meta.message_count, 3);
        assert_eq!(meta.tool_call_count, 1);
        assert_eq!(meta.total_prompt_tokens, 1000);
        assert_eq!(meta.total_completion_tokens, 200);
        assert_eq!(meta.total_cost, 0.03);
    }

    // ===== 空窗口：空会话 =====

    #[tokio::test]
    async fn fork_visible_empty_source_yields_empty_session() {
        // 源会话无消息 → 新会话存在但为空，parent=None 时为独立主会话（进主列表）
        let store = temp_store().await;
        let session = seed_source(&store).await;

        let new_id = store.fork_visible(&session.id, None).await.unwrap();

        let meta = store.get(&new_id).await.unwrap().unwrap();
        assert!(meta.parent_session_id.is_none());
        assert_eq!(meta.message_count, 0);
        assert!(store.load_full_history(&new_id).await.unwrap().is_empty());

        let list = store.list_all(None, 100, 0).await.unwrap();
        let ids: Vec<&str> = list.iter().map(|s| s.id.as_str()).collect();
        assert!(
            ids.contains(&new_id.as_str()),
            "parent=None 的独立会话应进主列表"
        );
    }

    // ===== 源会话不存在 =====

    #[tokio::test]
    async fn fork_visible_errors_when_source_missing() {
        let store = temp_store().await;
        let result = store
            .fork_visible("nonexistent", Some("p".to_string()))
            .await;
        assert!(matches!(result, Err(SessionError::NotFound(_))));
    }
}
