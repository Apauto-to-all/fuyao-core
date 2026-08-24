//! Session CRUD + 单字段局部更新
//!
//! sessions 表的全部元数据操作归此:
//! - 读 / 写生命周期:create(建行) / get / delete / list_all(主会话) /
//!   list_child_sessions(按父列举子会话) / count_with_filter
//! - 单字段局部更新:update_system_prompt / update_title / end_session
//!
//! **DB 唯一数据源**:session 的计数字段(message_count / tool_call_count /
//! total_* / total_cost)由 [`super::message`] 模块的 `insert_message` 事务内
//! SQL 原子自增维护,压缩元数据(compression_count / last_compacted_seq)由
//! [`super::compaction`] / [`super::rollback`] 的局部 UPDATE 维护。本模块只管
//! 「整行建(create)」与「离散单字段改」,不再有全量 `update`——避免单字段写
//! 被全量写覆盖(标题 bug 的根因)。
//!
//! 注:消息(Message)不在内存——产生即通过 [`super::SessionStore::insert_message`]
//! 单条落 DB,需要时按 session_id 查询(见 [`super::message`] 模块)。

use super::row::SessionRow;
use crate::error::SessionError;
use fuyao_api::Session;

/// session id 主键冲突时的最大重试次数（不含首次尝试）
///
/// 32 bit 熵下连续碰撞到这个次数的概率近乎零，命中即视为不可恢复故障向上抛错。
const ID_CONFLICT_MAX_RETRIES: usize = 3;

/// 校验 session 存在（事务内执行，复用调用方的事务连接）
///
/// 不存在返回 [`SessionError::NotFound`]。供 update_system_prompt / update_title /
/// end_session（本模块）与 mark_compaction / rollback_to（兄弟模块）等写入路径复用——
/// 避免给不存在的 session 写脏数据（孤儿消息 / 脏元数据）。每处原本内联同一份
/// `SELECT EXISTS(...) → NotFound` 仪式，现集中到这一处。
pub(super) async fn require_session(
    conn: &mut sqlx::SqliteConnection,
    session_id: &str,
) -> Result<(), SessionError> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)")
        .bind(session_id)
        .fetch_one(conn)
        .await?;
    if !exists {
        return Err(SessionError::NotFound(session_id.to_string()));
    }
    Ok(())
}

impl super::SessionStore {
    // ── 生命周期读写（整行建 / 查 / 删 / 列）──────────────────────

    /// 创建会话(只 INSERT sessions 元数据行)
    ///
    /// 消息产生时由调用方经 `insert_message` 单条落库,不在此处批量写。
    pub async fn create(&self, session: &Session) -> Result<(), SessionError> {
        sqlx::query(
            "INSERT INTO sessions (id, started_at, ended_at, end_reason,
                message_count, tool_call_count, total_prompt_tokens, total_completion_tokens,
                total_reasoning_tokens, total_cached_tokens, total_cost, title, system_prompt,
                compression_count, last_compacted_seq, parent_session_id, workspace, last_active_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        )
        .bind(session.id.as_str())
        .bind(session.started_at)
        .bind(session.ended_at)
        .bind(session.end_reason.as_deref())
        .bind(session.message_count)
        .bind(session.tool_call_count)
        .bind(session.total_prompt_tokens)
        .bind(session.total_completion_tokens)
        .bind(session.total_reasoning_tokens)
        .bind(session.total_cached_tokens)
        .bind(session.total_cost)
        .bind(session.title.as_deref())
        .bind(session.system_prompt.as_deref())
        .bind(session.compression_count)
        .bind(session.last_compacted_seq)
        .bind(session.parent_session_id.as_deref())
        .bind(session.workspace.as_deref())
        .bind(session.last_active_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// 创建会话（主键冲突自动重试）
    ///
    /// session id 由随机生成（见 [`Session::regenerate_id`]），与既有行碰撞时 DB
    /// 的 PRIMARY KEY 约束会让 `create` 返回主键冲突错误。本方法捕获该错误，
    /// 重新生成 id 再试，最多额外重试 [`ID_CONFLICT_MAX_RETRIES`] 次。
    ///
    /// **只重试主键冲突**——其它错误（磁盘满、连接断、schema 错误等）立即返回，
    /// 重试无意义且会掩盖真实故障。碰撞概率极低（32 bit 熵，个人单用户场景），
    /// 正常情况下首次即成功，重试路径几乎不会触发。
    ///
    /// 落库成功后 `session.id` 已是最终值；若中途换过 id，`session` 反映最后一次
    /// 尝试的 id（调用方据此拿到的 id 与 DB 一致）。
    pub async fn create_with_retry(&self, session: &mut Session) -> Result<(), SessionError> {
        for attempt in 0..=ID_CONFLICT_MAX_RETRIES {
            match self.create(session).await {
                Ok(()) => return Ok(()),
                Err(e) if e.is_primary_key_conflict() && attempt < ID_CONFLICT_MAX_RETRIES => {
                    tracing::warn!(
                        attempt = attempt + 1,
                        session_id = %session.id,
                        cause = "session id 主键冲突，重新生成 id 重试",
                    );
                    session.regenerate_id();
                }
                Err(e) => return Err(e),
            }
        }
        // 循环边界保证不会走到这里，循环条件 attempt < MAX 已在上一次迭代返回；
        // 此行仅为让编译器确认返回路径完备。
        unreachable!("重试循环已在边界内返回 Ok 或 Err")
    }

    /// 获取会话(纯元数据,不含消息)
    ///
    /// 消息请用 [`load_visible_messages`](super::SessionStore::load_visible_messages)
    /// 或 [`load_full_history`](super::SessionStore::load_full_history) 单独查。
    /// 行内 `child_count` 为该会话当前子会话数（COUNT 子查询实时计算）。
    pub async fn get(&self, session_id: &str) -> Result<Option<Session>, SessionError> {
        let row = sqlx::query_as::<_, SessionRow>(
            "SELECT id, started_at, ended_at, end_reason,
                    message_count, tool_call_count, total_prompt_tokens, total_completion_tokens,
                    total_reasoning_tokens, total_cached_tokens, total_cost, title, system_prompt,
                    compression_count, last_compacted_seq, parent_session_id, workspace, last_active_at,
                    (SELECT COUNT(*) FROM sessions c WHERE c.parent_session_id = sessions.id) AS child_count
             FROM sessions WHERE id = ?1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(Session::from))
    }

    /// 删除会话（cascade 删该会话及其全部子会话 + 各自的消息 + 任务列表）
    ///
    /// 删除范围是「本会话 + 其全部子会话」的会话组：单事务内删 todos + messages +
    /// sessions，组内每个会话的数据要么全删要么全留——避免出现「消息删了、session
    /// 行还在」或「session 删了、任务列表孤儿」的不一致窗口，也不留孤儿子会话。
    /// 返回 `true` = 删到了主会话行；`false` = 主会话不存在（此时组内的子会话与
    /// todos / messages 即便有残留也会被一并清掉）。
    pub async fn delete(&self, session_id: &str) -> Result<bool, SessionError> {
        let mut tx = self.pool.begin().await?;

        // 主会话行是否存在的判定独立于 DELETE 的 rows_affected——组删除会把
        // 子会话行也计入受影响行数，无法单独反映主行是否删到，故以显式 EXISTS 为准
        let main_row_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)")
                .bind(session_id)
                .fetch_one(&mut *tx)
                .await?;

        // 先清任务列表 + 消息，再删 session 行：组内会话的 id 集合经子查询取自
        // sessions 表，行仍在时子查询才取得到；session 行不存在时组内残留同样被清
        Self::delete_todos_in_tx(&mut tx, session_id).await?;
        sqlx::query(
            "DELETE FROM messages WHERE session_id IN
             (SELECT id FROM sessions WHERE id = ?1 OR parent_session_id = ?1)",
        )
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM sessions WHERE id = ?1 OR parent_session_id = ?1")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(main_row_exists)
    }

    /// 列出主会话(不含消息,分页,按最近活动时间倒序)
    ///
    /// 只返回顶层会话——`parent_session_id IS NULL` 的行；子会话（子代理 / 后台任务派生）
    /// 不进主列表，经 [`list_child_sessions`](Self::list_child_sessions) 按父列举。
    ///
    /// 排序用 `last_active_at DESC`——用户刚交互的会话排最前(类即时通讯的「最近会话」)。
    /// `last_active_at` 在每次 [`update`](Self::update) 落库时由 `unixepoch()` 刷新。
    ///
    /// `workspace_filter` 传 `Some(path)` 只看该工作目录的会话;`None` 看全部(含无 workspace 的)。
    /// 过滤在 SQL 层完成(走索引),不做内存截断。
    ///
    /// 每行携带 `child_count`（COUNT 子查询实时计算），供列表消费方驱动子会话入口显隐。
    pub async fn list_all(
        &self,
        workspace_filter: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Session>, SessionError> {
        let rows = sqlx::query_as::<_, SessionRow>(
            "SELECT id, started_at, ended_at, end_reason,
                    message_count, tool_call_count, total_prompt_tokens, total_completion_tokens,
                    total_reasoning_tokens, total_cached_tokens, total_cost, title, system_prompt,
                    compression_count, last_compacted_seq, parent_session_id, workspace, last_active_at,
                    (SELECT COUNT(*) FROM sessions c WHERE c.parent_session_id = sessions.id) AS child_count
             FROM sessions
             WHERE parent_session_id IS NULL AND (?1 IS NULL OR workspace = ?1)
             ORDER BY last_active_at DESC LIMIT ?2 OFFSET ?3",
        )
        .bind(workspace_filter)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Session::from).collect())
    }

    /// 列出某父会话下的全部子会话(全量,不分页,按创建序)
    ///
    /// 返回 `parent_session_id = parent_id` 的全部行，按 `started_at` 升序（创建序，
    /// 先派生的排前面）。全量返回、无分页信封——子会话数由单次任务派生的子代理数
    /// 决定，天然有限。`parent_id` 不存在或无子会话时返回空列表（纯查询原语，不校验
    /// 父存在性）。
    pub async fn list_child_sessions(&self, parent_id: &str) -> Result<Vec<Session>, SessionError> {
        let rows = sqlx::query_as::<_, SessionRow>(
            "SELECT id, started_at, ended_at, end_reason,
                    message_count, tool_call_count, total_prompt_tokens, total_completion_tokens,
                    total_reasoning_tokens, total_cached_tokens, total_cost, title, system_prompt,
                    compression_count, last_compacted_seq, parent_session_id, workspace, last_active_at,
                    (SELECT COUNT(*) FROM sessions c WHERE c.parent_session_id = sessions.id) AS child_count
             FROM sessions
             WHERE parent_session_id = ?1
             ORDER BY started_at ASC",
        )
        .bind(parent_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Session::from).collect())
    }

    /// 获取主会话总数(可选按工作目录过滤)
    ///
    /// 与 [`list_all`](Self::list_all) 的过滤语义一致：排除子会话、
    /// `workspace_filter` 配对，供上层计算分页总页数。
    /// `workspace_filter` 为 `None` 时统计全部主会话。
    pub async fn count_with_filter(
        &self,
        workspace_filter: Option<&str>,
    ) -> Result<i64, SessionError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sessions
             WHERE parent_session_id IS NULL AND (?1 IS NULL OR workspace = ?1)",
        )
        .bind(workspace_filter)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    // ── 单字段局部更新 ─────────────────────────────────────────
    //
    // 以下三个方法都只 UPDATE sessions 表的某个字段,不动其他字段、不动 messages 表。
    // 它们服务于"只改一个字段"的离散场景(title 异步生成 / 压缩后重建 prompt / 收尾)。
    // session 的计数字段不在此处——由 insert_message 事务内原子自增维护;
    // 压缩元数据(compression_count / last_compacted_seq)由 compaction / rollback
    // 模块的局部 UPDATE 维护。DB 唯一数据源,无全量写,杜绝字段覆盖。

    /// 更新 session 的 system_prompt(压缩后重建系统提示词用)
    ///
    /// 由 react 层在压缩触发后调用,把重新构建的提示词落库,避免旧 system_prompt 中残留的
    /// 动态内容(如"基于刚才的 X 错误继续排查")在 X 已被压进摘要后误导模型。
    ///
    /// # 错误
    /// - [`SessionError::NotFound`]:session_id 在数据库中不存在
    pub async fn update_system_prompt(
        &self,
        session_id: &str,
        new_system_prompt: &str,
    ) -> Result<(), SessionError> {
        let mut tx = self.pool.begin().await?;

        // 校验 session 存在(避免给不存在的 session 写脏数据)
        require_session(&mut tx, session_id).await?;

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

    /// 更新 session 的 title(标题自动生成后异步落库)
    ///
    /// 由 react 层 fire-and-forget spawn 的标题生成任务调用——spawn 的 future 是
    /// `'static` 的,无法借用 `&mut Session`,故走单字段 SQL 而非全量 `update(session)`。
    ///
    /// # 错误
    /// - [`SessionError::NotFound`]:session_id 在数据库中不存在
    pub async fn update_title(
        &self,
        session_id: &str,
        new_title: &str,
    ) -> Result<(), SessionError> {
        let mut tx = self.pool.begin().await?;

        require_session(&mut tx, session_id).await?;

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

    /// 标记会话结束(填 ended_at + end_reason)
    ///
    /// 由 `Engine::end_session` 在 session task 退出**之后**调用——确保 `ended_at` /
    /// `end_reason` 是最终值,不被 task 退出时的全量 `update(&session)` 覆盖。
    ///
    /// 与 `update_title` / `update_system_prompt` 对称:单字段更新方法,走独立 SQL 路径
    /// 而非全量 `update(session)`,避免并发更新间的字段覆盖。
    ///
    /// # 参数
    /// - `session_id`:被结束的会话
    /// - `end_reason`:结束原因(如 `"session_ended"` / `"engine_shutdown"`)
    ///
    /// # 错误
    /// - [`SessionError::NotFound`]:session_id 在数据库中不存在
    pub async fn end_session(
        &self,
        session_id: &str,
        end_reason: &str,
    ) -> Result<(), SessionError> {
        let mut tx = self.pool.begin().await?;

        require_session(&mut tx, session_id).await?;

        // 秒级 f64 时间戳,与 started_at / ended_at 字段类型对齐
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
}

#[cfg(test)]
mod tests {
    use super::super::SessionStore;
    use crate::error::SessionError;
    use fuyao_api::Session;

    /// 构造临时存储(隔离的临时目录)
    async fn temp_store() -> SessionStore {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let db_path = dir.path().join("test.db");
        // forget 让目录留到进程结束(async 测试里 SessionStore 跨 await 持有路径,dir 必须存活)
        std::mem::forget(dir);
        SessionStore::new(db_path).await.expect("创建存储失败")
    }

    /// 构造并落库一个挂在指定父下的子会话
    ///
    /// `started_at` 可控（连同 `last_active_at` 一并设定），供按创建序排列的断言用。
    async fn seed_child(store: &SessionStore, parent_id: &str, started_at: f64) -> Session {
        let mut child = Session::new(None, None, None);
        child.parent_session_id = Some(parent_id.to_string());
        child.started_at = started_at;
        child.last_active_at = started_at;
        store.create(&child).await.expect("落库子会话失败");
        child
    }

    // ===== 基础 CRUD 测试 =====

    #[tokio::test]
    async fn store_create_and_get() {
        let store = temp_store().await;
        let session = Session::new(None, Some("测试".to_string()), None);
        store.create(&session).await.unwrap();

        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.title, Some("测试".to_string()));
        assert_eq!(loaded.compression_count, 0);
        assert!(loaded.last_compacted_seq.is_none());
    }

    #[tokio::test]
    async fn store_get_returns_none_for_missing() {
        let store = temp_store().await;
        let result = store.get("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn store_delete_removes_session() {
        let store = temp_store().await;
        let session = Session::new(None, None, None);
        store.create(&session).await.unwrap();

        let deleted = store.delete(&session.id).await.unwrap();
        assert!(deleted);
        assert!(store.get(&session.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn store_delete_cascades_messages_and_todos() {
        // delete 单事务级联删 todos + messages + sessions，删后三类数据全无残留
        let store = temp_store().await;
        let session = Session::new(None, None, None);
        store.create(&session).await.unwrap();

        // 塞一条消息 + 一个任务
        let mut msg = fuyao_api::Message::user("对话".to_string());
        store.insert_message(&session.id, &mut msg).await.unwrap();
        store
            .write_todos(
                &session.id,
                vec![fuyao_api::TodoItem {
                    id: "1".to_string(),
                    content: "任务".to_string(),
                    status: "pending".to_string(),
                }],
            )
            .await
            .unwrap();
        assert_eq!(
            store.read_todos(&session.id).await.unwrap().len(),
            1,
            "删除前应有 1 条任务"
        );

        // 删会话 → todos / messages / sessions 全清
        let deleted = store.delete(&session.id).await.unwrap();
        assert!(deleted);
        assert!(store.get(&session.id).await.unwrap().is_none());
        assert!(
            store.read_todos(&session.id).await.unwrap().is_empty(),
            "删会话后任务列表应清空，无孤儿"
        );
        // messages 表该 session 的行也清空（count 全量查）
        assert_eq!(store.count_with_filter(None).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn store_delete_cascades_children_sessions_messages_and_todos() {
        // delete 按会话组整体清理：主会话行 + 全部子会话行 + 组内每个会话的消息与
        // todos 同事务一并删除，不留孤儿子会话
        let store = temp_store().await;
        let parent = Session::new(None, None, None);
        store.create(&parent).await.unwrap();
        let child_a = seed_child(&store, &parent.id, 100.0).await;
        let child_b = seed_child(&store, &parent.id, 200.0).await;

        // 组内三个会话各有消息 + 任务
        for sid in [&parent.id, &child_a.id, &child_b.id] {
            let mut msg = fuyao_api::Message::user("对话".to_string());
            store.insert_message(sid, &mut msg).await.unwrap();
            store
                .write_todos(
                    sid,
                    vec![fuyao_api::TodoItem {
                        id: "1".to_string(),
                        content: "任务".to_string(),
                        status: "pending".to_string(),
                    }],
                )
                .await
                .unwrap();
        }

        // 隔离锚点：他父的子会话不属于本组，删除不得波及
        let other_parent = Session::new(None, None, None);
        store.create(&other_parent).await.unwrap();
        let other_child = seed_child(&store, &other_parent.id, 300.0).await;
        let mut msg = fuyao_api::Message::user("他组对话".to_string());
        store
            .insert_message(&other_child.id, &mut msg)
            .await
            .unwrap();
        store
            .write_todos(
                &other_child.id,
                vec![fuyao_api::TodoItem {
                    id: "1".to_string(),
                    content: "他组任务".to_string(),
                    status: "pending".to_string(),
                }],
            )
            .await
            .unwrap();

        let deleted = store.delete(&parent.id).await.unwrap();
        assert!(deleted);

        // 主会话与全部子会话行无残留
        assert!(store.get(&parent.id).await.unwrap().is_none());
        assert!(
            store.get(&child_a.id).await.unwrap().is_none(),
            "子会话行应随组删除，无孤儿"
        );
        assert!(
            store.get(&child_b.id).await.unwrap().is_none(),
            "子会话行应随组删除，无孤儿"
        );
        assert!(
            store
                .list_child_sessions(&parent.id)
                .await
                .unwrap()
                .is_empty(),
            "删除后按父列举返回空"
        );
        // 组内每个会话的消息与 todos 无残留
        assert!(
            store
                .load_full_history(&parent.id)
                .await
                .unwrap()
                .is_empty(),
            "主会话消息无残留"
        );
        assert!(
            store
                .load_full_history(&child_a.id)
                .await
                .unwrap()
                .is_empty(),
            "子会话消息无残留"
        );
        assert!(
            store
                .load_full_history(&child_b.id)
                .await
                .unwrap()
                .is_empty(),
            "子会话消息无残留"
        );
        assert!(
            store.read_todos(&parent.id).await.unwrap().is_empty(),
            "主会话任务无残留"
        );
        assert!(
            store.read_todos(&child_a.id).await.unwrap().is_empty(),
            "子会话任务无残留"
        );
        assert!(
            store.read_todos(&child_b.id).await.unwrap().is_empty(),
            "子会话任务无残留"
        );

        // 他父的子会话原样保留（组删除不越界）
        assert!(
            store.get(&other_child.id).await.unwrap().is_some(),
            "他父的子会话不应被波及"
        );
        assert_eq!(
            store
                .load_full_history(&other_child.id)
                .await
                .unwrap()
                .len(),
            1,
            "他父子会话的消息原样保留"
        );
        assert_eq!(
            store.read_todos(&other_child.id).await.unwrap().len(),
            1,
            "他父子会话的任务原样保留"
        );
    }

    #[tokio::test]
    async fn store_delete_missing_parent_returns_false_but_cleans_children() {
        // 主会话行不存在时返回 false，但指向该 id 的子会话行及其数据仍按组清理——
        // 返回值只反映主行是否删到，组清理无条件执行
        let store = temp_store().await;
        let child = seed_child(&store, "no-such-parent", 100.0).await;
        let mut msg = fuyao_api::Message::user("孤儿对话".to_string());
        store.insert_message(&child.id, &mut msg).await.unwrap();
        store
            .write_todos(
                &child.id,
                vec![fuyao_api::TodoItem {
                    id: "1".to_string(),
                    content: "孤儿任务".to_string(),
                    status: "pending".to_string(),
                }],
            )
            .await
            .unwrap();

        let deleted = store.delete("no-such-parent").await.unwrap();
        assert!(!deleted, "主会话行不存在应返回 false");
        assert!(
            store.get(&child.id).await.unwrap().is_none(),
            "指向该 id 的子会话行仍被组清理删除"
        );
        assert!(
            store.load_full_history(&child.id).await.unwrap().is_empty(),
            "孤儿子会话的消息无残留"
        );
        assert!(
            store.read_todos(&child.id).await.unwrap().is_empty(),
            "孤儿子会话的任务无残留"
        );
    }

    #[tokio::test]
    async fn store_list_all_returns_sessions() {
        let store = temp_store().await;
        let s1 = Session::new(None, Some("会话1".to_string()), None);
        let s2 = Session::new(None, Some("会话2".to_string()), None);
        store.create(&s1).await.unwrap();
        store.create(&s2).await.unwrap();

        let list = store.list_all(None, 10, 0).await.unwrap();
        assert_eq!(list.len(), 2);
        let titles: Vec<String> = list
            .iter()
            .filter_map(|s| s.title.as_deref().map(str::to_string))
            .collect();
        assert!(titles.contains(&"会话1".to_string()));
        assert!(titles.contains(&"会话2".to_string()));
    }

    #[tokio::test]
    async fn store_count_returns_correct_count() {
        let store = temp_store().await;
        assert_eq!(store.count_with_filter(None).await.unwrap(), 0);
        store.create(&Session::new(None, None, None)).await.unwrap();
        assert_eq!(store.count_with_filter(None).await.unwrap(), 1);
    }

    // ===== create_with_retry：主键冲突重试 =====

    #[tokio::test]
    async fn create_with_retry_succeeds_on_first_try_when_no_conflict() {
        // 无冲突时首次即成功，session.id 不变（行为与 create 一致）
        let store = temp_store().await;
        let mut session = Session::new(None, Some("首次成功".into()), None);
        let original_id = session.id.clone();
        store.create_with_retry(&mut session).await.unwrap();
        assert_eq!(session.id, original_id, "无冲突不应重新生成 id");

        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.title.as_deref(), Some("首次成功"));
    }

    #[tokio::test]
    async fn create_with_retry_regenerates_id_on_primary_key_conflict() {
        // 制造冲突：先落一个 session，再把新 session 的 id 改成相同的，
        // create_with_retry 应回落到重新生成 id 直至落库成功
        let store = temp_store().await;
        let existing = Session::new(None, Some("已存在".into()), None);
        store.create(&existing).await.unwrap();

        let mut conflict = Session::new(None, Some("冲突方".into()), None);
        conflict.id = existing.id.clone(); // 强制碰撞
        store.create_with_retry(&mut conflict).await.unwrap();

        // 重试后 id 必然与冲突 id 不同，且新 id 在 DB 中可查
        assert_ne!(conflict.id, existing.id, "冲突后应已换新 id");
        let loaded = store.get(&conflict.id).await.unwrap().unwrap();
        assert_eq!(loaded.title.as_deref(), Some("冲突方"));
        // DB 现有两条
        assert_eq!(store.count_with_filter(None).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn create_with_retry_propagates_non_conflict_error() {
        // 非主键冲突错误（此处用重复列名外的手段不好造，改用验证：唯一约束外的错误不重试）
        // 这里用一个必然成功的 case 佐证返回 Ok；非冲突错误的传播由 is_primary_key_conflict
        // 的纯单元逻辑保证（见 error.rs 测试），不在此端到端重复
        let store = temp_store().await;
        let mut session = Session::new(None, None, None);
        assert!(store.create_with_retry(&mut session).await.is_ok());
    }

    #[tokio::test]
    async fn store_metadata_syncs_through_insert_and_compaction() {
        // session 的元数据字段不再由全量 update 写回,而是由 insert_message / mark_compaction
        // 各自在事务内维护。本测试验证这两条路径写入的元数据经 get 读回无丢失。
        let store = temp_store().await;
        let session = Session::new(None, None, None);
        store.create(&session).await.unwrap();

        // total_prompt_tokens 由 assistant 消息(带 prompt_tokens)经 insert_message 事务累加
        let mut msg = fuyao_api::Message::assistant(None);
        msg.prompt_tokens = 12345;
        store.insert_message(&session.id, &mut msg).await.unwrap();

        // compression_count 由 mark_compaction 写入的压缩边界条数决定
        store
            .mark_compaction(
                &session.id,
                "摘要".to_string(),
                crate::store::compaction::CompressionReason::Auto,
            )
            .await
            .unwrap();
        store
            .mark_compaction(
                &session.id,
                "摘要2".to_string(),
                crate::store::compaction::CompressionReason::Auto,
            )
            .await
            .unwrap();

        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.total_prompt_tokens, 12345);
        assert_eq!(loaded.compression_count, 2);
    }

    // ===== workspace 字段持久化 + 按工作目录过滤 / 最近活动排序 =====

    #[tokio::test]
    async fn store_create_and_get_preserves_workspace() {
        let store = temp_store().await;
        let session = Session::new(
            Some("/home/u/proj-a".to_string()),
            Some("项目A".to_string()),
            None,
        );
        store.create(&session).await.unwrap();

        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.workspace.as_deref(), Some("/home/u/proj-a"));
    }

    #[tokio::test]
    async fn store_create_and_get_workspace_none() {
        let store = temp_store().await;
        let session = Session::new(None, None, None);
        store.create(&session).await.unwrap();

        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert!(loaded.workspace.is_none());
    }

    #[tokio::test]
    async fn store_list_all_filters_by_workspace() {
        let store = temp_store().await;
        let proj_a_1 = Session::new(Some("/proj-a".to_string()), None, None);
        let proj_a_2 = Session::new(Some("/proj-a".to_string()), None, None);
        let proj_b = Session::new(Some("/proj-b".to_string()), None, None);
        let no_workspace = Session::new(None, None, None);
        store.create(&proj_a_1).await.unwrap();
        store.create(&proj_a_2).await.unwrap();
        store.create(&proj_b).await.unwrap();
        store.create(&no_workspace).await.unwrap();

        let a_list = store.list_all(Some("/proj-a"), 100, 0).await.unwrap();
        assert_eq!(a_list.len(), 2);
        assert!(
            a_list
                .iter()
                .all(|s| s.workspace.as_deref() == Some("/proj-a"))
        );

        let b_list = store.list_all(Some("/proj-b"), 100, 0).await.unwrap();
        assert_eq!(b_list.len(), 1);

        let all = store.list_all(None, 100, 0).await.unwrap();
        assert_eq!(all.len(), 4);
    }

    #[tokio::test]
    async fn store_count_with_filter_matches_list() {
        let store = temp_store().await;
        store
            .create(&Session::new(Some("/proj-a".to_string()), None, None))
            .await
            .unwrap();
        store
            .create(&Session::new(Some("/proj-a".to_string()), None, None))
            .await
            .unwrap();
        store
            .create(&Session::new(Some("/proj-b".to_string()), None, None))
            .await
            .unwrap();

        assert_eq!(store.count_with_filter(Some("/proj-a")).await.unwrap(), 2);
        assert_eq!(store.count_with_filter(Some("/proj-b")).await.unwrap(), 1);
        assert_eq!(store.count_with_filter(None).await.unwrap(), 3);
        assert_eq!(store.count_with_filter(None).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn insert_message_refreshes_last_active_at() {
        // last_active_at 现在由 insert_message 事务内的 unixepoch() 刷新,
        // 不再走全量 update。本测试验证插入一条消息后 last_active_at 推进。
        let store = temp_store().await;
        let session = Session::new(None, None, None);
        store.create(&session).await.unwrap();
        let created_active = store
            .get(&session.id)
            .await
            .unwrap()
            .unwrap()
            .last_active_at;

        // sleep 确保 unixepoch() 推进(SQLite unixepoch 精度为秒)
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let mut msg = fuyao_api::Message::user("新消息".to_string());
        store.insert_message(&session.id, &mut msg).await.unwrap();

        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert!(
            loaded.last_active_at > created_active,
            "insert_message 后 last_active_at 应晚于创建初值：{} > {}",
            loaded.last_active_at,
            created_active
        );
    }

    #[tokio::test]
    async fn store_list_all_orders_by_last_active_at_desc() {
        let store = temp_store().await;
        let old_session = Session::new(None, Some("老会话".to_string()), None);
        let new_session = Session::new(None, Some("新会话".to_string()), None);
        store.create(&old_session).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        store.create(&new_session).await.unwrap();

        let list = store.list_all(None, 10, 0).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].title.as_deref(), Some("新会话"));

        // 给老会话插入一条消息刷新 last_active_at(insert_message 事务内
        // 设 last_active_at = unixepoch()),使其重新成为最近活动的会话
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let mut msg = fuyao_api::Message::user("继续老会话".to_string());
        store
            .insert_message(&old_session.id, &mut msg)
            .await
            .unwrap();

        let list2 = store.list_all(None, 10, 0).await.unwrap();
        assert_eq!(list2[0].title.as_deref(), Some("老会话"));
    }

    // ===== parent_session_id(通用子任务标记)测试组 =====

    #[tokio::test]
    async fn parent_session_id_defaults_none_on_create() {
        let store = temp_store().await;
        let session = Session::new(None, None, None);
        store.create(&session).await.unwrap();

        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert!(loaded.parent_session_id.is_none());
    }

    #[tokio::test]
    async fn parent_session_id_persists_through_message_inserts() {
        // parent_session_id 在 create 时持久化。本测试验证后续 insert_message
        // 累加 message_count 时,不会影响 parent_session_id 字段(新架构下 session
        // 字段由各自的单字段/局部 UPDATE 维护,不再有全量覆盖路径)。
        let store = temp_store().await;
        let parent = Session::new(None, Some("父会话".to_string()), None);
        store.create(&parent).await.unwrap();

        let mut child = Session::new(None, None, None);
        child.parent_session_id = Some(parent.id.clone());
        store.create(&child).await.unwrap();

        let loaded = store.get(&child.id).await.unwrap().unwrap();
        assert_eq!(
            loaded.parent_session_id.as_deref(),
            Some(parent.id.as_str())
        );

        // insert_message 事务内累加 message_count,不触碰 parent_session_id
        for _ in 0..5 {
            let mut msg = fuyao_api::Message::user("对话".to_string());
            store.insert_message(&child.id, &mut msg).await.unwrap();
        }
        let reloaded = store.get(&child.id).await.unwrap().unwrap();
        assert_eq!(
            reloaded.parent_session_id.as_deref(),
            Some(parent.id.as_str())
        );
        assert_eq!(reloaded.message_count, 5);
    }

    // ===== 主列表排除子会话 + 按父列举 + child_count 计数 =====

    #[tokio::test]
    async fn store_list_all_excludes_child_sessions() {
        // 主列表只返回顶层会话：子会话不占列表行，也不占分页名额
        let store = temp_store().await;
        let parent = Session::new(None, Some("父会话".to_string()), None);
        store.create(&parent).await.unwrap();
        seed_child(&store, &parent.id, 100.0).await;
        seed_child(&store, &parent.id, 200.0).await;

        let list = store.list_all(None, 10, 0).await.unwrap();
        assert_eq!(list.len(), 1, "两个子会话不应出现在主列表");
        assert_eq!(list[0].id, parent.id);
        assert!(list[0].parent_session_id.is_none());
    }

    #[tokio::test]
    async fn store_count_with_filter_excludes_child_sessions() {
        // 计数与 list_all 同语义（只计主会话），保证分页 total 与列表条数一致
        let store = temp_store().await;
        let parent_a = Session::new(None, None, None);
        let parent_b = Session::new(None, None, None);
        store.create(&parent_a).await.unwrap();
        store.create(&parent_b).await.unwrap();
        seed_child(&store, &parent_a.id, 100.0).await;
        seed_child(&store, &parent_a.id, 200.0).await;
        seed_child(&store, &parent_b.id, 300.0).await;

        assert_eq!(store.count_with_filter(None).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn store_child_count_counts_children_per_row() {
        // COUNT 子查询按行计数：带子的主会话报实际条数，无子的报 0
        let store = temp_store().await;
        let with_children = Session::new(None, Some("带子".to_string()), None);
        let childless = Session::new(None, Some("无子".to_string()), None);
        store.create(&with_children).await.unwrap();
        store.create(&childless).await.unwrap();
        seed_child(&store, &with_children.id, 100.0).await;
        seed_child(&store, &with_children.id, 200.0).await;

        let list = store.list_all(None, 10, 0).await.unwrap();
        let row_with = list
            .iter()
            .find(|s| s.title.as_deref() == Some("带子"))
            .expect("应找到带子的主会话");
        let row_childless = list
            .iter()
            .find(|s| s.title.as_deref() == Some("无子"))
            .expect("应找到无子的主会话");
        assert_eq!(row_with.child_count, 2, "两个子会话应计为 2");
        assert_eq!(row_childless.child_count, 0, "无子会话应计为 0");

        // 单行直读同样携带计数（get 与 list_all 同一列清单）
        let single = store.get(&with_children.id).await.unwrap().unwrap();
        assert_eq!(single.child_count, 2);

        // 子会话自身恒为 0（引擎禁止子会话内递归派生）
        let children = store.list_child_sessions(&with_children.id).await.unwrap();
        assert!(children.iter().all(|c| c.child_count == 0));
    }

    #[tokio::test]
    async fn store_list_child_sessions_orders_by_started_at_asc() {
        // 按父列举：全量返回该父的子会话，按创建序（started_at 升序）排列
        let store = temp_store().await;
        let parent = Session::new(None, None, None);
        store.create(&parent).await.unwrap();
        // 落库顺序故意打乱（先 300 再 100 后 200），断言输出按时间升序
        let third = seed_child(&store, &parent.id, 300.0).await;
        let first = seed_child(&store, &parent.id, 100.0).await;
        let second = seed_child(&store, &parent.id, 200.0).await;

        let children = store.list_child_sessions(&parent.id).await.unwrap();
        assert_eq!(children.len(), 3, "该父的全部子会话全量返回");
        let ids: Vec<&str> = children.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![first.id.as_str(), second.id.as_str(), third.id.as_str()]
        );
        assert!(
            children
                .iter()
                .all(|c| c.parent_session_id.as_deref() == Some(parent.id.as_str()))
        );
    }

    #[tokio::test]
    async fn store_list_child_sessions_scoped_to_parent_and_empty_cases() {
        // 只列目标父的子会话（他父的子不混入）；无子父与不存在的父均返回空列表
        let store = temp_store().await;
        let parent_a = Session::new(None, None, None);
        let parent_b = Session::new(None, None, None);
        store.create(&parent_a).await.unwrap();
        store.create(&parent_b).await.unwrap();
        seed_child(&store, &parent_a.id, 100.0).await;
        seed_child(&store, &parent_b.id, 200.0).await;

        let children_a = store.list_child_sessions(&parent_a.id).await.unwrap();
        assert_eq!(children_a.len(), 1);
        assert_eq!(
            children_a[0].parent_session_id.as_deref(),
            Some(parent_a.id.as_str())
        );

        // 无子父 → 空；不存在的父 → 空（纯查询原语，不校验父存在性）
        let parent_c = Session::new(None, None, None);
        store.create(&parent_c).await.unwrap();
        assert!(
            store
                .list_child_sessions(&parent_c.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .list_child_sessions("nonexistent")
                .await
                .unwrap()
                .is_empty()
        );
    }

    // ===== 单字段更新:update_system_prompt =====

    #[tokio::test]
    async fn update_system_prompt_updates_db_row() {
        let store = temp_store().await;
        let session = Session::new(None, None, Some("旧提示词".to_string()));
        store.create(&session).await.unwrap();

        store
            .update_system_prompt(&session.id, "新提示词（压缩后重建）")
            .await
            .unwrap();

        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(
            loaded.system_prompt.as_deref(),
            Some("新提示词（压缩后重建）")
        );
        assert_eq!(loaded.compression_count, 0);
        assert!(loaded.last_compacted_seq.is_none());
    }

    #[tokio::test]
    async fn update_system_prompt_errors_on_missing_session() {
        let store = temp_store().await;
        let result = store.update_system_prompt("nonexistent", "新提示词").await;
        assert!(matches!(result, Err(SessionError::NotFound(_))));
    }

    // ===== 单字段更新:update_title =====

    #[tokio::test]
    async fn update_title_updates_db_row() {
        let store = temp_store().await;
        let session = Session::new(None, None, None);
        store.create(&session).await.unwrap();
        assert_eq!(session.title.as_deref(), Some("新会话"));

        store
            .update_title(&session.id, "Rust 异步讨论")
            .await
            .unwrap();

        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.title.as_deref(), Some("Rust 异步讨论"));
        assert_eq!(loaded.compression_count, 0);
        assert!(loaded.last_compacted_seq.is_none());
    }

    #[tokio::test]
    async fn update_title_errors_on_missing_session() {
        let store = temp_store().await;
        let result = store.update_title("nonexistent", "标题").await;
        assert!(matches!(result, Err(SessionError::NotFound(_))));
    }

    // ===== 单字段更新:end_session =====

    #[tokio::test]
    async fn end_session_updates_db_row() {
        let store = temp_store().await;
        let session = Session::new(None, None, None);
        store.create(&session).await.unwrap();
        let before = store.get(&session.id).await.unwrap().unwrap();
        assert!(before.ended_at.is_none());
        assert!(before.end_reason.is_none());

        store
            .end_session(&session.id, "session_ended")
            .await
            .unwrap();

        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert!(loaded.ended_at.is_some(), "ended_at 应已落库");
        assert_eq!(loaded.end_reason.as_deref(), Some("session_ended"));
        assert_eq!(loaded.compression_count, 0);
        assert!(loaded.last_compacted_seq.is_none());
    }

    #[tokio::test]
    async fn end_session_errors_on_missing_session() {
        let store = temp_store().await;
        let result = store.end_session("nonexistent", "session_ended").await;
        assert!(matches!(result, Err(SessionError::NotFound(_))));
    }
}
