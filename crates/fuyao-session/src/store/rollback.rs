//! 对话回退
//!
//! [`SessionStore::rollback_to`] 把会话回退到某个目标消息：删除目标 seq 之后的所有消息，
//! 重算被影响的 count 类字段与压缩元数据，保护消费类字段（token / cost）不动。
//!
//! # 回退语义
//!
//! 统一规则「删目标 seq 之后的所有消息，目标保留」，不分 user / compaction 分支——
//! 压缩消息只是「目标恰好是 compaction」或「被删范围含 compaction」的特例，统一规则天然覆盖。
//!
//! ## 回退点约束
//!
//! 只有 user 消息或 compaction 消息能作为回退目标：
//! - user 消息：自然语义「回到这条用户消息，从它重新开始」
//! - compaction 消息：自然语义「回到这次压缩完成的状态」
//!
//! assistant / tool 消息是中间态，回退到它们语义不完整（assistant 的 tool_call 可能无
//! 配对结果、tool 结果孤立存在会破坏配对），应拒绝。
//!
//! ## 字段处理
//!
//! | 字段类 | 处理 |
//! | ------ | ---- |
//! | count 类（message_count / tool_call_count） | **重算**（删了消息，现状必须与实际一致） |
//! | 压缩元数据（last_compacted_seq / compression_count） | **重算**（剩余消息重新统计） |
//! | 消费类（token / cost） | **不动**（账本记的是真实发生过的消费，回退不抹账） |
//!
//! 压缩元数据重算即重新统计剩余消息里的 compaction 情况：被删的 compaction 消息自然不计入，
//! `last_compacted_seq` 自动落到被删 compaction 消息的上一条（若无则置空）——这就是
//! 「动了压缩消息，统一回退到上一个压缩边界」的实现，无需特殊代码分支。
//!
//! # 单事务原子
//!
//! 全部步骤在一个 SQLite 事务内完成，避免中途失败留下脏状态。返回 [`RollbackPayload`]，
//! 供回退发起方（如活跃 session 回退的 `handle_control` 分支）就地刷新内存 session 对象，
//! 无需从 DB 重新 load；回退发起方把 payload 包装成 `RollbackMessage`（补 envelope）
//! 经统一消息管道发出，即为对外的 `OutputEvent::Rollback`。

use super::row::MessageRow;
use crate::error::SessionError;
use fuyao_api::MessageKind;
use fuyao_api::message::output::RollbackPayload;

impl super::SessionStore {
    /// 把会话回退到目标消息（删目标 seq 之后的所有消息 + 重算 count 类与压缩元数据）
    ///
    /// 单事务原子操作，五步：
    /// 1. 校验目标消息存在 + role/kind 合法（只能是 user 或 compaction）
    /// 2. 统计待删范围（`seq > target`）的分类计数，供返回值反馈
    /// 3. 删除目标 seq 之后的所有消息
    /// 4. 重算 count 类字段（message_count / tool_call_count）与压缩元数据
    ///    （last_compacted_seq / compression_count）
    /// 5. 局部 UPDATE sessions 写回 4 个重算字段——token/cost 原值不动
    ///
    /// # 参数
    /// - `session_id`:被回退的会话
    /// - `target_seq`:回退目标消息的 seq（目标本身保留，删它之后的）
    ///
    /// # 返回
    /// [`RollbackPayload`]，含锚点 seq、删除计数（界面通知用）、目标消息本体
    /// （user→Some 填输入框 / compaction→None）、重算后的 4 个状态字段（刷内存用）。
    /// 回退发起方把 payload 包装成 `RollbackMessage`（补 envelope）发出，即为
    /// `OutputEvent::Rollback`。
    ///
    /// # 错误
    /// - [`SessionError::NotFound`]:session_id 不存在，或 target_seq 在该 session 中无对应消息
    /// - [`SessionError::InvalidRollbackTarget`]:目标消息非 user 且非 compaction（assistant / tool 中间态）
    pub async fn rollback_to(
        &self,
        session_id: &str,
        target_seq: i64,
    ) -> Result<RollbackPayload, SessionError> {
        let mut tx = self.pool.begin().await?;

        // 1. 校验 session 存在（避免给不存在的 session 操作）
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)")
                .bind(session_id)
                .fetch_one(&mut *tx)
                .await?;
        if !exists {
            return Err(SessionError::NotFound(session_id.to_string()));
        }

        // 2. 取目标消息本身（提取 content/images 填输入框 + 拿 role/kind 校验）
        let target_row = sqlx::query_as::<_, MessageRow>(
            "SELECT id, session_id, model_id, role, content, images, tool_call_id,
                    tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                    reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind
             FROM messages WHERE session_id = ?1 AND seq = ?2",
        )
        .bind(session_id)
        .bind(target_seq)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(target_row) = target_row else {
            return Err(SessionError::NotFound(format!(
                "session={session_id} 中无 seq={target_seq} 的消息"
            )));
        };

        // 目标合法性以 kind 为准：compaction 消息在 DB 里 role='assistant'（由 mark_compaction
        // 硬编码），靠 role 判压缩会判错。合法目标 = user 消息 或 compaction 消息
        let is_user = target_row.role == "user";
        let is_compaction = target_row.kind == MessageKind::Compaction.as_str();
        if !is_user && !is_compaction {
            return Err(SessionError::InvalidRollbackTarget(format!(
                "seq={target_seq}（role={}, kind={}）",
                target_row.role, target_row.kind
            )));
        }

        // 目标是 user 消息：构造完整的 UserPayload 供前端填输入框（content + images 整体）
        // 目标是 compaction 消息：不填输入框，target_message 置 None
        let target_message: Option<fuyao_api::message::output::UserPayload> =
            if is_user {
                // images 在 DB 存 JSON 数组（[{mime_type, data}]），反序列化回 Vec
                let images = target_row
                .images
                .as_deref()
                .and_then(|s| match serde_json::from_str::<Vec<fuyao_api::ImageContent>>(s) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        tracing::warn!(cause = %e, "target images 反序列化失败，按空列表处理");
                        None
                    }
                })
                .unwrap_or_default();
                Some(RollbackPayload::user_payload_from(
                    target_row.content,
                    images,
                ))
            } else {
                None
            };

        // 3. 统计待删范围（seq > target）的分类计数
        //    deleted_count = user 消息数 + compaction 消息数（界面通知口径，不含 assistant/tool）
        //    deleted_total = 全部待删消息数（含 assistant/tool，审计用）
        let deleted_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages
             WHERE session_id = ?1 AND seq > ?2 AND (role = 'user' OR kind = 'compaction')",
        )
        .bind(session_id)
        .bind(target_seq)
        .fetch_one(&mut *tx)
        .await?;

        let deleted_total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND seq > ?2")
                .bind(session_id)
                .bind(target_seq)
                .fetch_one(&mut *tx)
                .await?;

        // 4. 删除目标 seq 之后的所有消息
        sqlx::query("DELETE FROM messages WHERE session_id = ?1 AND seq > ?2")
            .bind(session_id)
            .bind(target_seq)
            .execute(&mut *tx)
            .await?;

        // 5. 重算 count 类字段与压缩元数据（基于删除后的剩余消息）
        //    message_count：只数普通消息（kind='message'），与 count_messages 口径一致
        let message_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND kind = 'message'",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;

        //    tool_call_count：数 tool 结果消息数（一次调用对应一条 tool 结果）
        let tool_call_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND role = 'tool'",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;

        //    last_compacted_seq：剩余消息里最新一条 compaction 的 seq，无则置空
        let last_compacted_seq: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(seq) FROM messages WHERE session_id = ?1 AND kind = 'compaction'",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;

        //    compression_count：剩余消息里 compaction 的条数
        let compression_count: i32 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND kind = 'compaction'",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;

        // 6. 局部 UPDATE sessions：只写回 4 个重算字段，token/cost 原值不动
        //    （局部 UPDATE 是 session 表的唯一写范式——DB 单一数据源，无全量写）
        sqlx::query(
            "UPDATE sessions SET
                message_count = ?2, tool_call_count = ?3,
                compression_count = ?4, last_compacted_seq = ?5,
                last_active_at = unixepoch()
             WHERE id = ?1",
        )
        .bind(session_id)
        .bind(message_count)
        .bind(tool_call_count)
        .bind(compression_count)
        .bind(last_compacted_seq)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        tracing::info!(
            session_id = session_id,
            target_seq = target_seq,
            deleted_count = deleted_count,
            deleted_total = deleted_total,
            message_count = message_count,
            last_compacted_seq = ?last_compacted_seq,
            "对话已回退到目标消息"
        );

        Ok(RollbackPayload {
            target_seq,
            deleted_count,
            deleted_total,
            target_message,
            message_count,
            tool_call_count,
            last_compacted_seq,
            compression_count,
        })
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
        let mut msg = Message::tool_result(tool_call_id.to_string(), content.to_string());
        store.insert_message(sid, &mut msg).await.unwrap()
    }

    // ===== 回退到 user 消息 =====

    #[tokio::test]
    async fn rollback_to_user_deletes_subsequent_and_recounts() {
        // 场景：u1, a1, u2, a2 → 回退到 u2 → 删 a2 → message_count 从 4 变 3
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        insert_user(&store, &session.id, "u1").await; // seq 1
        insert_assistant(&store, &session.id, "a1").await; // seq 2
        let u2 = insert_user(&store, &session.id, "u2").await; // seq 3
        insert_assistant(&store, &session.id, "a2").await; // seq 4

        // 4 条普通消息经 insert_message 事务累加,message_count 已为 4
        // (无需手动设置——新架构下 message_count 由 insert_message 维护)
        let before = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(before.message_count, 4);

        let result = store.rollback_to(&session.id, u2).await.unwrap();

        // 目标 u2 保留，删了 a2（1 条 assistant）
        assert_eq!(result.target_seq, u2);
        assert_eq!(
            result.deleted_count, 0,
            "删的 a2 是 assistant，不计入 deleted_count"
        );
        assert_eq!(result.deleted_total, 1, "deleted_total 含 assistant");
        let target = result
            .target_message
            .as_ref()
            .expect("user 目标应有 target_message");
        assert_eq!(target.content, "u2", "user 目标 → content = u2");
        assert!(target.images.is_empty());

        // message_count 重算：剩余 u1, a1, u2 → 4 条里 3 条是 message kind
        assert_eq!(result.message_count, 3);
        assert_eq!(result.tool_call_count, 0);

        // DB 状态：只剩 3 条消息
        let full = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(full.len(), 3);
        assert_eq!(full[2].content.as_deref(), Some("u2"), "最后一条是 u2");

        // session 元数据已写回
        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.message_count, 3);
    }

    #[tokio::test]
    async fn rollback_to_user_with_tool_calls_recounts_tool_count() {
        // 场景：u1, a1(带 tool_call), t1(结果), u2, a2(带 tool_call), t2(结果)
        //       → 回退到 u2 → 删 a2, t2 → tool_call_count 从 2 变 1
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        insert_user(&store, &session.id, "u1").await; // seq 1
        let mut a1 = Message::assistant(None);
        a1.tool_calls = Some(
            serde_json::json!([{"id": "call_1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]),
        );
        store.insert_message(&session.id, &mut a1).await.unwrap(); // seq 2
        insert_tool(&store, &session.id, "call_1", "结果1").await; // seq 3
        let u2 = insert_user(&store, &session.id, "u2").await; // seq 4
        let mut a2 = Message::assistant(None);
        a2.tool_calls = Some(
            serde_json::json!([{"id": "call_2", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]),
        );
        store.insert_message(&session.id, &mut a2).await.unwrap(); // seq 5
        insert_tool(&store, &session.id, "call_2", "结果2").await; // seq 6

        let result = store.rollback_to(&session.id, u2).await.unwrap();

        // 删了 a2, t2 → tool_call_count 从 2 变 1
        assert_eq!(result.tool_call_count, 1, "删了一条 tool 结果，剩余 1");
        assert_eq!(result.deleted_total, 2, "删了 a2 + t2 共 2 条");
        assert_eq!(result.deleted_count, 0, "a2/t2 都不是 user/compaction");
    }

    // ===== 回退到 compaction 消息 =====

    #[tokio::test]
    async fn rollback_to_compaction_keeps_target_and_metadata() {
        // 场景：u1, a1, [compaction@seq3], u2, a2 → 回退到 compaction@seq3
        //       → 删 u2, a2 → compaction 保留，last_compacted_seq 仍指向它
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        insert_user(&store, &session.id, "u1").await; // seq 1
        insert_assistant(&store, &session.id, "a1").await; // seq 2
        let comp_seq = store
            .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
            .await
            .unwrap(); // seq 3
        insert_user(&store, &session.id, "u2").await; // seq 4
        insert_assistant(&store, &session.id, "a2").await; // seq 5

        let result = store.rollback_to(&session.id, comp_seq).await.unwrap();

        assert_eq!(result.target_seq, comp_seq);
        assert!(
            result.target_message.is_none(),
            "compaction 目标 → target_message 置空（不填输入框）"
        );
        // 删了 u2, a2 → deleted_count=1（u2 是 user），deleted_total=2
        assert_eq!(result.deleted_count, 1);
        assert_eq!(result.deleted_total, 2);

        // compaction 保留，last_compacted_seq 仍指向它，compression_count 仍 1
        assert_eq!(result.last_compacted_seq, Some(comp_seq));
        assert_eq!(result.compression_count, 1);

        // message_count 重算：剩余 u1, a1, compaction → 2 条 message kind（排除 compaction）
        assert_eq!(result.message_count, 2);

        // compaction 消息仍在
        let full = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(full.len(), 3);
        assert_eq!(full[2].kind, MessageKind::Compaction);
    }

    // ===== 回退跨压缩边界 =====

    #[tokio::test]
    async fn rollback_across_compaction_boundary_falls_back_metadata() {
        // 场景：u1, [c1@2], u2, [c2@4], u3, a3
        //       → 回退到 u2@seq3 → 删 c2, u3, a3 → last_compacted_seq 落到 c1
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        insert_user(&store, &session.id, "u1").await; // seq 1
        let c1 = store
            .mark_compaction(&session.id, "摘要1".to_string(), CompressionReason::Auto)
            .await
            .unwrap(); // seq 2
        let u2 = insert_user(&store, &session.id, "u2").await; // seq 3
        let c2 = store
            .mark_compaction(&session.id, "摘要2".to_string(), CompressionReason::Auto)
            .await
            .unwrap(); // seq 4
        insert_user(&store, &session.id, "u3").await; // seq 5
        insert_assistant(&store, &session.id, "a3").await; // seq 6

        let result = store.rollback_to(&session.id, u2).await.unwrap();

        // 删了 c2, u3, a3 → deleted_count=2（c2 是 compaction + u3 是 user），deleted_total=3
        assert_eq!(result.deleted_count, 2);
        assert_eq!(result.deleted_total, 3);

        // last_compacted_seq 落到 c1，compression_count 从 2 变 1
        assert_eq!(result.last_compacted_seq, Some(c1));
        assert_eq!(result.compression_count, 1);

        // 剩余：u1, c1, u2 → c2 已删
        let full = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(full.len(), 3);
        assert!(full.iter().all(|m| m.seq <= u2));
        assert!(!full.iter().any(|m| m.seq == c2), "c2 应在删除范围内被清理");
    }

    #[tokio::test]
    async fn rollback_across_all_compactions_clears_metadata() {
        // 场景：[c1@1], u1, a1, [c2@4], u2 → 回退到 u1@seq2 → 删 c2, u2
        //       （注：c1@seq1 < u1@seq2，不在删除范围）
        //       改造场景：u1, [c1@2], [c2@3], u2 → 回退到 u1 → 删 c1, c2, u2 → 全清
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        let u1 = insert_user(&store, &session.id, "u1").await; // seq 1
        store
            .mark_compaction(&session.id, "摘要1".to_string(), CompressionReason::Auto)
            .await
            .unwrap(); // seq 2
        store
            .mark_compaction(&session.id, "摘要2".to_string(), CompressionReason::Auto)
            .await
            .unwrap(); // seq 3
        insert_user(&store, &session.id, "u2").await; // seq 4

        let result = store.rollback_to(&session.id, u1).await.unwrap();

        // 删了 c1, c2, u2 → deleted_count=3（2 compaction + 1 user），deleted_total=3
        assert_eq!(result.deleted_count, 3);
        assert_eq!(result.deleted_total, 3);

        // 所有 compaction 都被删 → last_compacted_seq 置 None，compression_count 归 0
        assert_eq!(result.last_compacted_seq, None);
        assert_eq!(result.compression_count, 0);

        // 剩余：只有 u1
        let full = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(full.len(), 1);
        assert_eq!(full[0].content.as_deref(), Some("u1"));
    }

    // ===== 目标合法性校验 =====

    #[tokio::test]
    async fn rollback_rejects_assistant_target() {
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();
        insert_user(&store, &session.id, "u1").await; // seq 1
        let a1 = insert_assistant(&store, &session.id, "a1").await; // seq 2

        let result = store.rollback_to(&session.id, a1).await;
        assert!(
            matches!(result, Err(SessionError::InvalidRollbackTarget(_))),
            "assistant 中间态不可作为回退目标"
        );

        // 消息未被删
        let full = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(full.len(), 2, "校验失败不应改动 DB");
    }

    #[tokio::test]
    async fn rollback_rejects_tool_target() {
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();
        insert_user(&store, &session.id, "u1").await; // seq 1
        let t_seq = insert_tool(&store, &session.id, "call_1", "结果").await; // seq 2

        let result = store.rollback_to(&session.id, t_seq).await;
        assert!(
            matches!(result, Err(SessionError::InvalidRollbackTarget(_))),
            "tool 孤儿消息不可作为回退目标"
        );
    }

    // ===== 目标 / session 不存在 =====

    #[tokio::test]
    async fn rollback_errors_when_session_missing() {
        let store = temp_store().await;
        let result = store.rollback_to("nonexistent", 1).await;
        assert!(matches!(result, Err(SessionError::NotFound(_))));
    }

    #[tokio::test]
    async fn rollback_errors_when_target_seq_missing() {
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();
        insert_user(&store, &session.id, "u1").await; // seq 1

        // seq=99 不存在
        let result = store.rollback_to(&session.id, 99).await;
        assert!(matches!(result, Err(SessionError::NotFound(_))));
    }

    // ===== 消费类字段保护 =====

    #[tokio::test]
    async fn rollback_preserves_token_and_cost_fields() {
        // 回退只改消息列表,不抹消费账:token/cost 原值保留。
        // 消费类字段现由 insert_message 事务内累加(assistant 消息贡献 token/cost),
        // 这里插入一条带消费的 assistant 消息构造真实消费,再回退删它,验证账本不动。
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        let u1 = insert_user(&store, &session.id, "u1").await; // seq 1
        let mut a1 = Message::assistant(None);
        a1.prompt_tokens = 12345;
        a1.completion_tokens = 678;
        a1.reasoning_tokens = 100;
        a1.cached_tokens = 200;
        a1.cost = 0.05;
        store.insert_message(&session.id, &mut a1).await.unwrap(); // seq 2

        let result = store.rollback_to(&session.id, u1).await.unwrap();

        // 返回值不含消费字段——它们在 DB 原值保留
        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(
            loaded.total_prompt_tokens, 12345,
            "total_prompt_tokens 不动"
        );
        assert_eq!(loaded.total_completion_tokens, 678);
        assert_eq!(loaded.total_reasoning_tokens, 100);
        assert_eq!(loaded.total_cached_tokens, 200);
        assert_eq!(loaded.total_cost, 0.05, "total_cost 不动");

        // count 类字段已重算（u1 保留，a1 删除）
        assert_eq!(loaded.message_count, result.message_count);
        assert_eq!(loaded.tool_call_count, result.tool_call_count);
    }

    // ===== 边界：回退到最后一条消息（无可删内容） =====

    #[tokio::test]
    async fn rollback_to_last_message_deletes_nothing() {
        // 目标就是最新消息 → seq 之后无消息 → deleted_*=0，count 不变
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();
        let u1 = insert_user(&store, &session.id, "u1").await; // seq 1

        let result = store.rollback_to(&session.id, u1).await.unwrap();

        assert_eq!(result.deleted_count, 0);
        assert_eq!(result.deleted_total, 0);
        assert_eq!(result.message_count, 1);
        let target = result
            .target_message
            .as_ref()
            .expect("user 目标应有 target_message");
        assert_eq!(target.content, "u1");
        assert!(target.images.is_empty());
    }

    // ===== target_message 完整性：content + images 整体保留 =====

    #[tokio::test]
    async fn rollback_target_message_preserves_content_and_images() {
        // user 消息带图片回填到输入框时，content + images 都应完整带回（整体不拆散）
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        let img = fuyao_api::ImageContent {
            mime_type: "image/png".to_string(),
            data: "aGVsbG8=".to_string(),
        };
        let mut m1 = Message::user_with_images("看图".to_string(), vec![img.clone()]);
        store.insert_message(&session.id, &mut m1).await.unwrap();
        insert_assistant(&store, &session.id, "回复").await;

        let result = store.rollback_to(&session.id, m1.seq).await.unwrap();
        let target = result
            .target_message
            .as_ref()
            .expect("user 目标应有 target_message");
        assert_eq!(target.content, "看图");
        assert_eq!(target.images, vec![img], "images 应完整保留");
    }
}
