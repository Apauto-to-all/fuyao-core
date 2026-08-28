//! 对话派生（fork）
//!
//! [`SessionStore::fork_to`] 把会话派生到某个目标消息**之前**：以目标之前的全部消息
//! 为快照，复制进一个新独立 session，源会话不动任何数据。
//!
//! # fork 点约束
//!
//! 只有 user 消息或 compaction 消息能作为 fork 目标：
//! - user 消息：分支停在发送这条用户消息之前的时刻，供调用方改写后重发
//! - compaction 消息：分支废弃这次压缩及之后的所有消息
//!
//! assistant / tool 消息是中间态，切到它们语义不完整（assistant 的 tool_call 可能
//! 无配对结果、tool 结果孤立存在会破坏配对），拒绝。
//!
//! # 复制语义
//!
//! - **新 session**：独立主会话（`parent_session_id = None`），随机 id（冲突重试）；
//!   workspace 与 system_prompt 复制源持久化原值（源提示词可能已被压缩重建过，
//!   重建值才是模型看到的）；标题取默认；统计从 0 起算
//! - **消息**：`seq < target` 的全部消息（含压缩前旧消息与更早的 compaction 边界）
//!   按 seq 升序整批复制，seq 从 1 起连续重新分配
//! - **元数据**：count 类（message_count / tool_call_count）、压缩元数据
//!   （last_compacted_seq / compression_count）、消费类（token 四项 / cost）全部按
//!   复制结果聚合重算——复制完全等于「在新会话重新产生这些消息」
//!
//! # 与回退的行为关系
//!
//! 回退与派生共用同一套目标校验与保留范围（`seq < target`）：同一目标点上，派生
//! 分支的消息集即回退后源会话的剩余消息集；差别仅在破坏性——回退删源会话的目标
//! 及其后消息，派生把目标之前的消息落成新分支、源会话原样不动。
//!
//! # 单事务原子
//!
//! 校验 + 建行 + 复制 + 重算在一个 SQLite 事务内完成，中途失败整体回滚，
//! 不留半成品分支。
//!
//! # 与运行态的关系
//!
//! 消息 seq 单调递增且插入后不改，活跃 turn 的落库只发生在目标之后——分支快照
//! （`seq < target`）不受源会话并发写影响，派生无需先停 turn。

use super::row::MessageRow;
use crate::error::SessionError;
use fuyao_api::Message;

impl super::SessionStore {
    /// 把会话派生到目标消息之前（复制 seq < target 的全部消息到新独立 session）
    ///
    /// 单事务原子操作，五步：
    /// 1. 取源 session 行（workspace / system_prompt），不存在报 NotFound
    /// 2. 校验 fork 目标：存在且为 user 消息或 compaction 消息
    ///    （复用 [`super::rollback::validate_cut_target`] 共用校验）
    /// 3. 构造新 session 行落库（独立主会话，随机 id，主键冲突重新生成重试）
    /// 4. 复制 seq < target 的全部消息（按 seq 升序，新 seq 从事务内起点连续分配）
    /// 5. 按复制结果聚合重算新会话的 count 类 / 压缩元数据 / 消费类字段
    ///
    /// # 参数
    /// - `session_id`:源会话（本操作不动其任何数据）
    /// - `target_seq`:fork 目标消息的 seq（该消息及其后的消息不进分支）
    ///
    /// # 返回
    /// 新会话 id。分支的后续状态经既有读路径获取：`load_full_history` /
    /// `list_messages_before` 看分支消息，`get` 看分支元数据；继续对话由
    /// 上层经运行时门面的 resume 路径激活分支。
    ///
    /// # 错误
    /// - [`SessionError::NotFound`]:session_id 不存在，或 target_seq 在该 session 中
    ///   无对应消息
    /// - [`SessionError::InvalidCutTarget`]:目标非 user 且非 compaction
    ///   （assistant / tool 中间态）
    pub async fn fork_to(&self, session_id: &str, target_seq: i64) -> Result<String, SessionError> {
        let mut tx = self.pool.begin().await?;

        // 1. 取源 session 行：分支继承 workspace 与 system_prompt
        let source_row: Option<(Option<String>, Option<String>)> =
            sqlx::query_as("SELECT workspace, system_prompt FROM sessions WHERE id = ?1")
                .bind(session_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((workspace, system_prompt)) = source_row else {
            return Err(SessionError::NotFound(session_id.to_string()));
        };

        // 2. 校验 fork 目标：存在且为 user 消息或 compaction 消息
        super::rollback::validate_cut_target(&mut tx, session_id, target_seq).await?;

        // 3. 构造新 session 行并落库：独立主会话（parent=None），计数字段从 0 起算
        //    （第 5 步按复制结果重算到位）。随机 id 主键冲突时重新生成重试，
        //    仅重试主键冲突——其它错误（磁盘满、连接断等）重试无意义且掩盖真实故障
        let mut new_session = super::session::new_session(workspace, None, system_prompt);
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
                        new_session.id = super::session::generate_id();
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        // 4. 复制 seq < target 的全部消息到新会话（含压缩前旧消息与更早的
        //    compaction 边界）：seq 从事务内 COALESCE(MAX(seq), 0) + 1 起点连续
        //    递增分配
        let rows: Vec<MessageRow> = sqlx::query_as::<_, MessageRow>(
            "SELECT id, session_id, model_id, role, content, images, tool_call_id,
                    tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                    reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind
             FROM messages WHERE session_id = ?1 AND seq < ?2 ORDER BY seq",
        )
        .bind(session_id)
        .bind(target_seq)
        .fetch_all(&mut *tx)
        .await?;
        let copied: Vec<Message> = rows.into_iter().map(Message::from).collect();

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

        // 5. 按复制结果聚合重算新会话的全部派生字段（新会话行从 0 起算，聚合即终值）：
        //    message_count 只数 kind='message' 的普通消息，tool_call_count 数其中
        //    role=tool 的 tool 结果；消费类（token 四项 / cost）只计普通消息各自
        //    携带的值（user/tool 消息恒为 0）；压缩元数据按复制进来的 compaction
        //    边界在新 seq 空间下重新统计（MAX 落到分支里最新一条 compaction 的
        //    **新** seq，无则置空）
        let (
            message_count,
            tool_call_count,
            prompt_sum,
            completion_sum,
            reasoning_sum,
            cached_sum,
            cost_sum,
            last_compacted_seq,
            compression_count,
        ): (i64, i64, i64, i64, i64, i64, f64, Option<i64>, i32) = sqlx::query_as(
            "SELECT
                COUNT(*) FILTER (WHERE kind = 'message'),
                COUNT(*) FILTER (WHERE role = 'tool'),
                COALESCE(SUM(prompt_tokens) FILTER (WHERE kind = 'message'), 0),
                COALESCE(SUM(completion_tokens) FILTER (WHERE kind = 'message'), 0),
                COALESCE(SUM(reasoning_tokens) FILTER (WHERE kind = 'message'), 0),
                COALESCE(SUM(cached_tokens) FILTER (WHERE kind = 'message'), 0),
                COALESCE(SUM(cost) FILTER (WHERE kind = 'message'), 0.0),
                MAX(seq) FILTER (WHERE kind = 'compaction'),
                COUNT(*) FILTER (WHERE kind = 'compaction')
             FROM messages WHERE session_id = ?1",
        )
        .bind(&new_session.id)
        .fetch_one(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE sessions SET
                message_count = ?2, tool_call_count = ?3,
                total_prompt_tokens = ?4, total_completion_tokens = ?5,
                total_reasoning_tokens = ?6, total_cached_tokens = ?7,
                total_cost = ?8, compression_count = ?9, last_compacted_seq = ?10,
                last_active_at = unixepoch()
             WHERE id = ?1",
        )
        .bind(&new_session.id)
        .bind(message_count)
        .bind(tool_call_count)
        .bind(prompt_sum)
        .bind(completion_sum)
        .bind(reasoning_sum)
        .bind(cached_sum)
        .bind(cost_sum)
        .bind(compression_count)
        .bind(last_compacted_seq)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        tracing::info!(
            source_session_id = session_id,
            new_session_id = %new_session.id,
            target_seq = target_seq,
            message_count = message_count,
            "对话已派生到目标消息之前（新独立会话）"
        );

        Ok(new_session.id)
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

    /// 落库一个带 workspace 与 system_prompt 的源会话
    async fn seed_source(store: &SessionStore) -> fuyao_api::Session {
        store
            .create_session(
                Some("/proj".to_string()),
                None,
                Some("系统提示词".to_string()),
            )
            .await
            .unwrap()
    }

    // ===== fork 到 user 消息 =====

    #[tokio::test]
    async fn fork_to_user_target_copies_before_target_and_keeps_source() {
        // 场景：u1, a1, u2, a2 → fork 到 u2 → 分支 = [u1, a1]（seq 重排为 1, 2），源会话原样
        let store = temp_store().await;
        let session = seed_source(&store).await;

        insert_user(&store, &session.id, "u1").await; // seq 1
        insert_assistant(&store, &session.id, "a1").await; // seq 2
        let u2 = insert_user(&store, &session.id, "u2").await; // seq 3
        insert_assistant(&store, &session.id, "a2").await; // seq 4

        let branch_id = store.fork_to(&session.id, u2).await.unwrap();

        // 分支：目标 u2 与其后的 a2 不进分支，seq 从 1 连续重分配
        let branch = store.load_full_history(&branch_id).await.unwrap();
        assert_eq!(branch.len(), 2);
        assert_eq!(branch[0].content.as_deref(), Some("u1"));
        assert_eq!(branch[0].seq, 1);
        assert_eq!(branch[1].content.as_deref(), Some("a1"));
        assert_eq!(branch[1].seq, 2);

        // 分支继承 workspace 与 system_prompt，是独立主会话（parent = None）
        let branch_meta = store.get(&branch_id).await.unwrap().unwrap();
        assert_eq!(branch_meta.workspace.as_deref(), Some("/proj"));
        assert_eq!(branch_meta.system_prompt.as_deref(), Some("系统提示词"));
        assert!(branch_meta.parent_session_id.is_none());
        assert_eq!(branch_meta.message_count, 2);

        // 源会话原样不动：4 条消息、计数不变
        let source = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(source.len(), 4);
        let source_meta = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(source_meta.message_count, 4);
    }

    #[tokio::test]
    async fn fork_to_user_with_tool_calls_recounts_tool_count() {
        // 场景：u1, a1(tool_call), t1, u2, a2(tool_call), t2 → fork 到 u2
        //       → 分支 = [u1, a1, t1]，tool_call_count = 1
        let store = temp_store().await;
        let session = seed_source(&store).await;

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

        let branch_id = store.fork_to(&session.id, u2).await.unwrap();

        let branch_meta = store.get(&branch_id).await.unwrap().unwrap();
        assert_eq!(branch_meta.tool_call_count, 1, "只复制了一个 tool 结果");
        assert_eq!(branch_meta.message_count, 3, "剩余 u1, a1, t1");
        let branch = store.load_full_history(&branch_id).await.unwrap();
        assert_eq!(branch.len(), 3);
    }

    // ===== fork 到 compaction 消息 =====

    #[tokio::test]
    async fn fork_to_compaction_target_excludes_it_and_clears_metadata() {
        // 场景：u1, a1, [compaction@3], u2 → fork 到 compaction
        //       → 分支 = [u1, a1]，压缩消息本体与其后的 u2 不进分支，元数据空
        let store = temp_store().await;
        let session = seed_source(&store).await;

        insert_user(&store, &session.id, "u1").await; // seq 1
        insert_assistant(&store, &session.id, "a1").await; // seq 2
        let comp_seq = store
            .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
            .await
            .unwrap(); // seq 3
        insert_user(&store, &session.id, "u2").await; // seq 4

        let branch_id = store.fork_to(&session.id, comp_seq).await.unwrap();

        let branch = store.load_full_history(&branch_id).await.unwrap();
        assert_eq!(branch.len(), 2, "压缩消息本体与其后的 u2 不进分支");
        assert!(
            !branch
                .iter()
                .any(|m| m.kind == fuyao_api::MessageKind::Compaction),
            "分支里不应有压缩消息"
        );

        let branch_meta = store.get(&branch_id).await.unwrap().unwrap();
        assert_eq!(branch_meta.last_compacted_seq, None);
        assert_eq!(branch_meta.compression_count, 0);
        assert_eq!(branch_meta.message_count, 2);

        // 源会话的压缩元数据原样不动
        let source_meta = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(source_meta.last_compacted_seq, Some(comp_seq));
        assert_eq!(source_meta.compression_count, 1);
    }

    #[tokio::test]
    async fn fork_to_across_boundary_falls_back_compaction_metadata() {
        // 场景：u1, [c1@2], u2, [c2@4], u3 → fork 到 u2@3
        //       → 分支 = [u1, c1]，last_compacted_seq 落到 c1 的**新** seq（2）
        let store = temp_store().await;
        let session = seed_source(&store).await;

        insert_user(&store, &session.id, "u1").await; // seq 1
        store
            .mark_compaction(&session.id, "摘要1".to_string(), CompressionReason::Auto)
            .await
            .unwrap(); // seq 2 = c1
        let u2 = insert_user(&store, &session.id, "u2").await; // seq 3
        store
            .mark_compaction(&session.id, "摘要2".to_string(), CompressionReason::Auto)
            .await
            .unwrap(); // seq 4 = c2
        insert_user(&store, &session.id, "u3").await; // seq 5

        let branch_id = store.fork_to(&session.id, u2).await.unwrap();

        // 分支含 c1（新 seq = 2），c2 与 u3 不进分支
        let branch = store.load_full_history(&branch_id).await.unwrap();
        assert_eq!(branch.len(), 2);
        assert_eq!(branch[1].kind, fuyao_api::MessageKind::Compaction);
        assert_eq!(branch[1].seq, 2, "压缩边界 seq 已在新会话空间重分配");

        let branch_meta = store.get(&branch_id).await.unwrap().unwrap();
        assert_eq!(branch_meta.last_compacted_seq, Some(2));
        assert_eq!(branch_meta.compression_count, 1);
    }

    // ===== 消费类字段按复制结果聚合 =====

    #[tokio::test]
    async fn fork_to_accumulates_consumption_of_copied_messages() {
        // 分支的消费类字段 = 复制进来的 assistant 消息各自携带的 token/cost 之和
        // （复制等于重新产生这些消息）；目标之后的消费不进分支；源会话消费不动
        let store = temp_store().await;
        let session = seed_source(&store).await;

        insert_user(&store, &session.id, "u1").await; // seq 1
        let mut a1 = Message::assistant(None);
        a1.prompt_tokens = 1000;
        a1.completion_tokens = 200;
        a1.cost = 0.03;
        store.insert_message(&session.id, &mut a1).await.unwrap(); // seq 2
        let u2 = insert_user(&store, &session.id, "u2").await; // seq 3
        let mut a2 = Message::assistant(None);
        a2.prompt_tokens = 5000;
        a2.completion_tokens = 800;
        a2.cost = 0.2;
        store.insert_message(&session.id, &mut a2).await.unwrap(); // seq 4

        let branch_id = store.fork_to(&session.id, u2).await.unwrap();

        let branch_meta = store.get(&branch_id).await.unwrap().unwrap();
        assert_eq!(branch_meta.total_prompt_tokens, 1000, "只计复制进来的 a1");
        assert_eq!(branch_meta.total_completion_tokens, 200);
        assert_eq!(branch_meta.total_cost, 0.03);

        // 源会话消费账本不动
        let source_meta = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(source_meta.total_prompt_tokens, 6000);
        assert_eq!(source_meta.total_completion_tokens, 1000);
    }

    // ===== 边界：fork 到首条消息（分支为空）=====

    #[tokio::test]
    async fn fork_to_first_message_yields_empty_branch() {
        // fork 到首条消息 → 分支为空会话（与回退删到空对应），元数据归零
        let store = temp_store().await;
        let session = seed_source(&store).await;

        let u1 = insert_user(&store, &session.id, "u1").await; // seq 1
        insert_assistant(&store, &session.id, "a1").await; // seq 2

        let branch_id = store.fork_to(&session.id, u1).await.unwrap();

        let branch = store.load_full_history(&branch_id).await.unwrap();
        assert!(branch.is_empty(), "分支应为空");
        let branch_meta = store.get(&branch_id).await.unwrap().unwrap();
        assert_eq!(branch_meta.message_count, 0);
        assert_eq!(branch_meta.compression_count, 0);
        assert!(branch_meta.last_compacted_seq.is_none());
    }

    // ===== 目标合法性校验 =====

    #[tokio::test]
    async fn fork_to_rejects_assistant_target_and_creates_nothing() {
        // assistant 中间态不可作为 fork 目标；校验失败不产生任何新会话
        let store = temp_store().await;
        let session = seed_source(&store).await;
        insert_user(&store, &session.id, "u1").await; // seq 1
        let a1 = insert_assistant(&store, &session.id, "a1").await; // seq 2

        let before = store.count_with_filter(None).await.unwrap();
        let result = store.fork_to(&session.id, a1).await;
        assert!(
            matches!(result, Err(SessionError::InvalidCutTarget(_))),
            "assistant 中间态不可作为 fork 目标"
        );
        assert_eq!(
            store.count_with_filter(None).await.unwrap(),
            before,
            "校验失败不应新建会话"
        );
    }

    #[tokio::test]
    async fn fork_to_rejects_tool_target() {
        let store = temp_store().await;
        let session = seed_source(&store).await;
        insert_user(&store, &session.id, "u1").await; // seq 1
        let t_seq = insert_tool(&store, &session.id, "call_1", "结果").await; // seq 2

        let result = store.fork_to(&session.id, t_seq).await;
        assert!(
            matches!(result, Err(SessionError::InvalidCutTarget(_))),
            "tool 孤儿消息不可作为 fork 目标"
        );
    }

    // ===== 目标 / session 不存在 =====

    #[tokio::test]
    async fn fork_to_errors_when_session_missing() {
        let store = temp_store().await;
        let result = store.fork_to("nonexistent", 1).await;
        assert!(matches!(result, Err(SessionError::NotFound(_))));
    }

    #[tokio::test]
    async fn fork_to_errors_when_target_seq_missing() {
        let store = temp_store().await;
        let session = seed_source(&store).await;
        insert_user(&store, &session.id, "u1").await; // seq 1

        let result = store.fork_to(&session.id, 99).await;
        assert!(matches!(result, Err(SessionError::NotFound(_))));
    }

    // ===== 分支进入主列表（独立会话可见性）=====

    #[tokio::test]
    async fn fork_to_branch_appears_in_main_list_as_independent_session() {
        let store = temp_store().await;
        let session = seed_source(&store).await;
        insert_user(&store, &session.id, "u1").await; // seq 1
        let u2 = insert_user(&store, &session.id, "u2").await; // seq 2

        let branch_id = store.fork_to(&session.id, u2).await.unwrap();

        let list = store.list_all(None, 100, 0).await.unwrap();
        let ids: Vec<&str> = list.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&session.id.as_str()));
        assert!(ids.contains(&branch_id.as_str()), "分支应作为主会话可见");
        assert_eq!(list.len(), 2);
    }
}
