//! 对话回退
//!
//! [`SessionStore::rollback_to`] 把会话回退到某个目标消息**之前**：删除目标消息及其后
//! 的所有消息，重算被影响的 count 类字段与压缩元数据，保护消费类字段（token / cost）不动。
//!
//! # 回退语义
//!
//! 统一规则「删目标消息及其后的所有消息」，不分 user / compaction 分支——
//! 压缩消息只是「目标恰好是 compaction」或「被删范围含 compaction」的特例，统一规则天然覆盖。
//!
//! ## 回退点约束
//!
//! 只有 user 消息或 compaction 消息能作为回退目标：
//! - user 消息：自然语义「删掉这条用户消息及之后，回到发送它之前的时刻」
//! - compaction 消息：自然语义「废弃这次压缩及之后的所有消息」
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
//! `last_compacted_seq` 自动落到剩余消息里最新一条 compaction（若无则置空）——这就是
//! 「动了压缩消息，统一回退到上一个压缩边界」的实现，无需特殊代码分支。
//!
//! # 单事务原子
//!
//! 全部步骤在一个 SQLite 事务内完成，避免中途失败留下脏状态。变更本体无返回载荷：
//! 回退是纯存储变更，调用方需要的后续状态（剩余消息、重算后的会话元数据）一律经
//! 既有读路径获取——`load_full_history` 看剩余消息，`get` 看 session 行。

use crate::error::SessionError;
use fuyao_api::MessageKind;

/// 校验切割目标消息（事务内执行，复用调用方的事务连接）
///
/// 目标必须存在且为 user 消息（role 判定）或 compaction 消息（kind 判定）。
/// assistant / tool 消息是中间态，切到它们语义不完整（assistant 的 tool_call 可能
/// 无配对结果、tool 结果孤立存在会破坏配对），拒绝。
///
/// 回退与派生两条切割路径共用本校验，保证两个入口的目标约束永远同一套。
///
/// # 错误
/// - [`SessionError::NotFound`]:目标 seq 在该 session 中无对应消息
/// - [`SessionError::InvalidCutTarget`]:目标非 user 且非 compaction
pub(super) async fn validate_cut_target(
    conn: &mut sqlx::SqliteConnection,
    session_id: &str,
    target_seq: i64,
) -> Result<(), SessionError> {
    // 取目标消息的 role/kind 做合法性判定
    let target_row: Option<(String, String)> =
        sqlx::query_as("SELECT role, kind FROM messages WHERE session_id = ?1 AND seq = ?2")
            .bind(session_id)
            .bind(target_seq)
            .fetch_optional(conn)
            .await?;

    let Some((role, kind)) = target_row else {
        return Err(SessionError::NotFound(format!(
            "session={session_id} 中无 seq={target_seq} 的消息"
        )));
    };

    // 目标合法性判定：role 管对话角色，kind 管消息类型标记，两者正交。
    // 合法切割目标 = user 消息（role 判定） 或 compaction 消息（kind 判定）。
    // compaction 消息的 role 是 assistant（摘要由助手产出，以 assistant 身份
    // 参与对话流），但它是压缩边界这一事实只由 kind 表达——故 compaction
    // 一律靠 kind 识别，与 role 无关。
    if role != "user" && kind != MessageKind::Compaction.as_str() {
        return Err(SessionError::InvalidCutTarget(format!(
            "seq={target_seq}（role={role}, kind={kind}）"
        )));
    }
    Ok(())
}

impl super::SessionStore {
    /// 把会话回退到目标消息之前（删目标消息及其后的所有消息 + 重算 count 类与压缩元数据）
    ///
    /// 单事务原子操作，四步：
    /// 1. 校验目标消息存在 + role/kind 合法（只能是 user 或 compaction）
    /// 2. 删除目标消息及其后的所有消息——`RETURNING` 带回被删行的 (role, kind)，
    ///    Rust 侧聚合成删除计数写进日志
    /// 3. 重算 count 类字段（message_count / tool_call_count）与压缩元数据
    ///    （last_compacted_seq / compression_count）——四个标量聚合进单条查询
    /// 4. 局部 UPDATE sessions 写回 4 个重算字段——token/cost 原值不动
    ///
    /// # 参数
    /// - `session_id`:被回退的会话
    /// - `target_seq`:回退目标消息的 seq（目标及其后的消息一并删除）
    ///
    /// # 返回
    /// `Ok(())`——无返回载荷。回退后的会话状态经读路径获取：
    /// `load_full_history` 看剩余消息，`get` 看重算后的 session 行。
    ///
    /// # 错误
    /// - [`SessionError::NotFound`]:session_id 不存在，或 target_seq 在该 session 中无对应消息
    /// - [`SessionError::InvalidCutTarget`]:目标消息非 user 且非 compaction（assistant / tool 中间态）
    pub async fn rollback_to(&self, session_id: &str, target_seq: i64) -> Result<(), SessionError> {
        let mut tx = self.pool.begin().await?;

        // 1. 校验 session 存在（避免给不存在的 session 操作）
        super::session::require_session(&mut tx, session_id).await?;

        // 2. 校验回退目标：存在且为 user 消息或 compaction 消息
        validate_cut_target(&mut tx, session_id, target_seq).await?;

        // 3. 删除目标消息及其后的所有消息，RETURNING 带回被删行的 (role, kind)——
        //    删除与删除计数一次往返完成。计数仅用于日志（INFO 重建故事）：
        //    deleted_count = user 消息数 + compaction 消息数（不含 assistant/tool）
        //    deleted_total = 全部被删消息数（含 assistant/tool）
        let deleted_rows: Vec<(String, String)> = sqlx::query_as(
            "DELETE FROM messages WHERE session_id = ?1 AND seq >= ?2 RETURNING role, kind",
        )
        .bind(session_id)
        .bind(target_seq)
        .fetch_all(&mut *tx)
        .await?;
        let deleted_total = deleted_rows.len() as i64;
        let deleted_count = deleted_rows
            .iter()
            // user 靠 role 判定，compaction 靠 kind 判定（compaction 行的 role 是 assistant）
            .filter(|(role, kind)| role == "user" || kind == MessageKind::Compaction.as_str())
            .count() as i64;

        // 4. 重算 count 类字段与压缩元数据（基于删除后的剩余消息），四个标量聚合成单条查询
        //    message_count：只数普通消息（kind='message'）
        //    tool_call_count：数 tool 结果消息数（一次调用对应一条 tool 结果）
        //    last_compacted_seq：剩余消息里最新一条 compaction 的 seq，无则置空（MAX 空集为 NULL）
        //    compression_count：剩余消息里 compaction 的条数
        let (message_count, tool_call_count, last_compacted_seq, compression_count): (
            i64,
            i64,
            Option<i64>,
            i32,
        ) = sqlx::query_as(
            "SELECT
                COUNT(*) FILTER (WHERE kind = 'message'),
                COUNT(*) FILTER (WHERE role = 'tool'),
                MAX(seq) FILTER (WHERE kind = 'compaction'),
                COUNT(*) FILTER (WHERE kind = 'compaction')
             FROM messages WHERE session_id = ?1",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;

        // 5. 局部 UPDATE sessions：只写回 4 个重算字段，token/cost 原值不动
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
            "对话已回退到目标消息之前"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::SessionStore;
    use crate::error::SessionError;
    use crate::store::compaction::CompressionReason;
    use fuyao_api::Message;

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

    // ===== 回退到 user 消息 =====

    #[tokio::test]
    async fn rollback_to_user_deletes_target_and_after_recounts() {
        // 场景：u1, a1, u2, a2 → 回退到 u2 → 删 u2, a2 → message_count 从 4 变 2
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();

        insert_user(&store, &session.id, "u1").await; // seq 1
        insert_assistant(&store, &session.id, "a1").await; // seq 2
        let u2 = insert_user(&store, &session.id, "u2").await; // seq 3
        insert_assistant(&store, &session.id, "a2").await; // seq 4

        // 4 条普通消息经 insert_message 事务累加,message_count 已为 4
        // (无需手动设置——新架构下 message_count 由 insert_message 维护)
        let before = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(before.message_count, 4);

        store.rollback_to(&session.id, u2).await.unwrap();

        // DB 状态：目标 u2 与其后的 a2 一并删除，只剩 u1, a1
        let full = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(full.len(), 2);
        assert_eq!(full[1].content.as_deref(), Some("a1"), "最后一条是 a1");

        // session 元数据已重算并写回：2 条 message kind，无 tool 结果
        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.message_count, 2);
        assert_eq!(loaded.tool_call_count, 0);
    }

    #[tokio::test]
    async fn rollback_to_user_with_tool_calls_recounts_tool_count() {
        // 场景：u1, a1(带 tool_call), t1(结果), u2, a2(带 tool_call), t2(结果)
        //       → 回退到 u2 → 删 u2, a2, t2 → tool_call_count 从 2 变 1
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();

        insert_user(&store, &session.id, "u1").await; // seq 1
        let mut a1 = Message::assistant(None);
        a1.tool_calls = Some(vec![fuyao_api::ToolCallData {
            id: "call_1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        }]);
        store.insert_message(&session.id, &mut a1).await.unwrap(); // seq 2
        insert_tool(&store, &session.id, "call_1", "结果1").await; // seq 3
        let u2 = insert_user(&store, &session.id, "u2").await; // seq 4
        let mut a2 = Message::assistant(None);
        a2.tool_calls = Some(vec![fuyao_api::ToolCallData {
            id: "call_2".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        }]);
        store.insert_message(&session.id, &mut a2).await.unwrap(); // seq 5
        insert_tool(&store, &session.id, "call_2", "结果2").await; // seq 6

        store.rollback_to(&session.id, u2).await.unwrap();

        // 删了 u2, a2, t2 → tool_call_count 从 2 变 1，剩余 3 条消息
        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.tool_call_count, 1, "删了一条 tool 结果，剩余 1");
        assert_eq!(loaded.message_count, 3, "剩余 u1, a1, t1");
        let full = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(full.len(), 3, "删了 u2 + a2 + t2 共 3 条");
    }

    // ===== 回退到 compaction 消息 =====

    #[tokio::test]
    async fn rollback_to_compaction_discards_it_and_clears_metadata() {
        // 场景：u1, a1, [compaction@seq3], u2, a2 → 回退到 compaction@seq3
        //       → 删 compaction, u2, a2 → 压缩元数据清空
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();

        insert_user(&store, &session.id, "u1").await; // seq 1
        insert_assistant(&store, &session.id, "a1").await; // seq 2
        let comp_seq = store
            .mark_compaction(
                &session.id,
                "摘要".to_string(),
                None,
                CompressionReason::Auto,
            )
            .await
            .unwrap(); // seq 3
        insert_user(&store, &session.id, "u2").await; // seq 4
        insert_assistant(&store, &session.id, "a2").await; // seq 5

        store.rollback_to(&session.id, comp_seq).await.unwrap();

        // compaction 本体被删，压缩元数据随之清空
        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.last_compacted_seq, None);
        assert_eq!(loaded.compression_count, 0);

        // message_count 重算：剩余 u1, a1 → 2 条 message kind
        assert_eq!(loaded.message_count, 2);

        // 剩余消息里已无 compaction
        let full = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(full.len(), 2);
        assert!(
            !full.iter().any(|m| m.seq == comp_seq),
            "compaction 本体应被删除"
        );
    }

    // ===== 回退跨压缩边界 =====

    #[tokio::test]
    async fn rollback_across_compaction_boundary_falls_back_metadata() {
        // 场景：u1, [c1@2], u2, [c2@4], u3, a3
        //       → 回退到 u2@seq3 → 删 u2, c2, u3, a3 → last_compacted_seq 落到 c1
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();

        insert_user(&store, &session.id, "u1").await; // seq 1
        let c1 = store
            .mark_compaction(
                &session.id,
                "摘要1".to_string(),
                None,
                CompressionReason::Auto,
            )
            .await
            .unwrap(); // seq 2
        let u2 = insert_user(&store, &session.id, "u2").await; // seq 3
        let c2 = store
            .mark_compaction(
                &session.id,
                "摘要2".to_string(),
                None,
                CompressionReason::Auto,
            )
            .await
            .unwrap(); // seq 4
        insert_user(&store, &session.id, "u3").await; // seq 5
        insert_assistant(&store, &session.id, "a3").await; // seq 6

        store.rollback_to(&session.id, u2).await.unwrap();

        // last_compacted_seq 落到 c1，compression_count 从 2 变 1
        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.last_compacted_seq, Some(c1));
        assert_eq!(loaded.compression_count, 1);

        // 剩余：u1, c1 → c2 已删
        let full = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(full.len(), 2);
        assert!(full.iter().all(|m| m.seq <= c1));
        assert!(!full.iter().any(|m| m.seq == c2), "c2 应在删除范围内被清理");
    }

    #[tokio::test]
    async fn rollback_across_all_compactions_clears_metadata() {
        // 场景：u1, [c1@2], [c2@3], u2 → 回退到 u1 → 全删（含 u1 本体）→ 全清
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();

        let u1 = insert_user(&store, &session.id, "u1").await; // seq 1
        store
            .mark_compaction(
                &session.id,
                "摘要1".to_string(),
                None,
                CompressionReason::Auto,
            )
            .await
            .unwrap(); // seq 2
        store
            .mark_compaction(
                &session.id,
                "摘要2".to_string(),
                None,
                CompressionReason::Auto,
            )
            .await
            .unwrap(); // seq 3
        insert_user(&store, &session.id, "u2").await; // seq 4

        store.rollback_to(&session.id, u1).await.unwrap();

        // 回退到首条：连 u1 一起删，会话消息清空，所有元数据归零
        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.last_compacted_seq, None);
        assert_eq!(loaded.compression_count, 0);
        assert_eq!(loaded.message_count, 0);

        let full = store.load_full_history(&session.id).await.unwrap();
        assert!(full.is_empty(), "回退到首条应删到空");
    }

    // ===== 目标合法性校验 =====

    #[tokio::test]
    async fn rollback_rejects_assistant_target() {
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();
        insert_user(&store, &session.id, "u1").await; // seq 1
        let a1 = insert_assistant(&store, &session.id, "a1").await; // seq 2

        let result = store.rollback_to(&session.id, a1).await;
        assert!(
            matches!(result, Err(SessionError::InvalidCutTarget(_))),
            "assistant 中间态不可作为回退目标"
        );

        // 消息未被删
        let full = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(full.len(), 2, "校验失败不应改动 DB");
    }

    #[tokio::test]
    async fn rollback_rejects_tool_target() {
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();
        insert_user(&store, &session.id, "u1").await; // seq 1
        let t_seq = insert_tool(&store, &session.id, "call_1", "结果").await; // seq 2

        let result = store.rollback_to(&session.id, t_seq).await;
        assert!(
            matches!(result, Err(SessionError::InvalidCutTarget(_))),
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
        let session = store.create_session(None, None, None).await.unwrap();
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
        let session = store.create_session(None, None, None).await.unwrap();

        let u1 = insert_user(&store, &session.id, "u1").await; // seq 1
        let mut a1 = Message::assistant(None);
        a1.prompt_tokens = 12345;
        a1.completion_tokens = 678;
        a1.reasoning_tokens = 100;
        a1.cached_tokens = 200;
        a1.cost = 0.05;
        store.insert_message(&session.id, &mut a1).await.unwrap(); // seq 2

        store.rollback_to(&session.id, u1).await.unwrap();

        // 消费账本不动——token/cost 原值保留
        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(
            loaded.total_prompt_tokens, 12345,
            "total_prompt_tokens 不动"
        );
        assert_eq!(loaded.total_completion_tokens, 678);
        assert_eq!(loaded.total_reasoning_tokens, 100);
        assert_eq!(loaded.total_cached_tokens, 200);
        assert_eq!(loaded.total_cost, 0.05, "total_cost 不动");

        // count 类字段已重算（u1 与 a1 一并删除，消息清空）
        assert_eq!(loaded.message_count, 0);
        assert_eq!(loaded.tool_call_count, 0);
    }

    // ===== 边界：回退到会话唯一一条消息（删到空） =====

    #[tokio::test]
    async fn rollback_to_only_message_empties_session() {
        // 目标就是最新且唯一消息 → 连它一起删 → 会话消息清空，元数据归零
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();
        let u1 = insert_user(&store, &session.id, "u1").await; // seq 1

        store.rollback_to(&session.id, u1).await.unwrap();

        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.message_count, 0);
        let full = store.load_full_history(&session.id).await.unwrap();
        assert!(full.is_empty(), "唯一消息也应被删除");
    }
}
