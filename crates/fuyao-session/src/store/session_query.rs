//! Session 查询面（单行读 / 列举 / 计数）
//!
//! [`SessionStore`] 的只读查询原语归此，与写操作面（`session` 模块的
//! create_session / delete / update_session）分离：
//! - [`SessionStore::get`]：单行读（附实时 child_count）
//! - [`SessionStore::list_all`]：主会话分页列举（最近活动倒序 + 工作目录过滤）
//! - [`SessionStore::list_child_sessions`]：按父列举子会话（创建序全量）
//! - [`SessionStore::count_with_filter`]：主会话计数（与 list_all 同过滤语义）

use super::row::SessionRow;
use crate::error::SessionError;
use fuyao_api::Session;

impl super::SessionStore {
    /// 获取会话(纯元数据,不含消息)
    ///
    /// 消息请用 [`load_visible_messages`](super::SessionStore::load_visible_messages)
    /// 或 [`load_full_history`](super::SessionStore::load_full_history) 单独查。
    /// 行内 `child_count` 为该会话当前子会话数（COUNT 子查询实时计算）。
    pub async fn get(&self, session_id: &str) -> Result<Option<Session>, SessionError> {
        let row = sqlx::query_as::<_, SessionRow>(
            "SELECT id, started_at,
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

    /// 列出主会话(不含消息,分页,按最近活动时间倒序)
    ///
    /// 只返回顶层会话——`parent_session_id IS NULL` 的行；子会话（子代理 / 后台任务派生）
    /// 不进主列表，经 [`list_child_sessions`](Self::list_child_sessions) 按父列举。
    ///
    /// 排序用 `last_active_at DESC`——用户刚交互的会话排最前(类即时通讯的「最近会话」)。
    /// `last_active_at` 在每次 insert_message 落库时由 `unixepoch()` 刷新。
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
            "SELECT id, started_at,
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
            "SELECT id, started_at,
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
}

#[cfg(test)]
mod tests {
    use super::super::SessionStore;
    use fuyao_api::Session;

    /// 构造临时存储(隔离的临时目录)
    async fn temp_store() -> SessionStore {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let db_path = dir.path().join("test.db");
        // forget 让目录留到进程结束(async 测试里 SessionStore 跨 await 持有路径,dir 必须存活)
        std::mem::forget(dir);
        SessionStore::new(db_path).await.expect("创建存储失败")
    }

    /// 以自定义字段构造并落库一个会话（复用 session 模块的内部构造点 + 建行写入体）
    async fn seed_session(store: &SessionStore, mutate: impl FnOnce(&mut Session)) -> Session {
        let mut session = super::super::session::new_session(None, None, None);
        mutate(&mut session);
        SessionStore::insert_session_row(&store.pool, &session)
            .await
            .expect("落库会话失败");
        session
    }

    /// 构造并落库一个挂在指定父下的子会话
    ///
    /// `started_at` 可控（连同 `last_active_at` 一并设定），供按创建序排列的断言用。
    async fn seed_child(store: &SessionStore, parent_id: &str, started_at: f64) -> Session {
        seed_session(store, |s| {
            s.parent_session_id = Some(parent_id.to_string());
            s.started_at = started_at;
            s.last_active_at = started_at;
        })
        .await
    }

    // ===== get：单行读 =====

    #[tokio::test]
    async fn store_get_returns_none_for_missing() {
        let store = temp_store().await;
        let result = store.get("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn store_create_and_get_preserves_workspace() {
        let store = temp_store().await;
        let session = store
            .create_session(Some("/home/u/proj-a".to_string()), None, None)
            .await
            .unwrap();

        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.workspace.as_deref(), Some("/home/u/proj-a"));
    }

    #[tokio::test]
    async fn store_create_and_get_workspace_none() {
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();

        let loaded = store.get(&session.id).await.unwrap().unwrap();
        assert!(loaded.workspace.is_none());
    }

    // ===== list_all：主列表 + 工作目录过滤 =====

    #[tokio::test]
    async fn store_list_all_returns_sessions() {
        let store = temp_store().await;
        seed_session(&store, |s| s.title = Some("会话1".to_string())).await;
        seed_session(&store, |s| s.title = Some("会话2".to_string())).await;

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
    async fn store_list_all_filters_by_workspace() {
        let store = temp_store().await;
        store
            .create_session(Some("/proj-a".to_string()), None, None)
            .await
            .unwrap();
        store
            .create_session(Some("/proj-a".to_string()), None, None)
            .await
            .unwrap();
        store
            .create_session(Some("/proj-b".to_string()), None, None)
            .await
            .unwrap();
        store.create_session(None, None, None).await.unwrap();

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
    async fn store_list_all_orders_by_last_active_at_desc() {
        let store = temp_store().await;
        let old_session = seed_session(&store, |s| s.title = Some("老会话".to_string())).await;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        seed_session(&store, |s| s.title = Some("新会话".to_string())).await;

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

    #[tokio::test]
    async fn insert_message_refreshes_last_active_at() {
        // last_active_at 由 insert_message 事务内的 unixepoch() 刷新。
        // 本测试验证插入一条消息后 last_active_at 推进。
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();
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

    // ===== count_with_filter：与 list_all 同过滤语义 =====

    #[tokio::test]
    async fn store_count_returns_correct_count() {
        let store = temp_store().await;
        assert_eq!(store.count_with_filter(None).await.unwrap(), 0);
        store.create_session(None, None, None).await.unwrap();
        assert_eq!(store.count_with_filter(None).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn store_count_with_filter_matches_list() {
        let store = temp_store().await;
        store
            .create_session(Some("/proj-a".to_string()), None, None)
            .await
            .unwrap();
        store
            .create_session(Some("/proj-a".to_string()), None, None)
            .await
            .unwrap();
        store
            .create_session(Some("/proj-b".to_string()), None, None)
            .await
            .unwrap();

        assert_eq!(store.count_with_filter(Some("/proj-a")).await.unwrap(), 2);
        assert_eq!(store.count_with_filter(Some("/proj-b")).await.unwrap(), 1);
        assert_eq!(store.count_with_filter(None).await.unwrap(), 3);
    }

    // ===== 主列表排除子会话 + 按父列举 + child_count 计数 =====

    #[tokio::test]
    async fn store_list_all_excludes_child_sessions() {
        // 主列表只返回顶层会话：子会话不占列表行，也不占分页名额
        let store = temp_store().await;
        let parent = store.create_session(None, None, None).await.unwrap();
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
        let parent_a = store.create_session(None, None, None).await.unwrap();
        let parent_b = store.create_session(None, None, None).await.unwrap();
        seed_child(&store, &parent_a.id, 100.0).await;
        seed_child(&store, &parent_a.id, 200.0).await;
        seed_child(&store, &parent_b.id, 300.0).await;

        assert_eq!(store.count_with_filter(None).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn store_child_count_counts_children_per_row() {
        // COUNT 子查询按行计数：带子的主会话报实际条数，无子的报 0
        let store = temp_store().await;
        let with_children = seed_session(&store, |s| s.title = Some("带子".to_string())).await;
        seed_session(&store, |s| s.title = Some("无子".to_string())).await;
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
        let parent = store.create_session(None, None, None).await.unwrap();
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
        let parent_a = store.create_session(None, None, None).await.unwrap();
        let parent_b = store.create_session(None, None, None).await.unwrap();
        seed_child(&store, &parent_a.id, 100.0).await;
        seed_child(&store, &parent_b.id, 200.0).await;

        let children_a = store.list_child_sessions(&parent_a.id).await.unwrap();
        assert_eq!(children_a.len(), 1);
        assert_eq!(
            children_a[0].parent_session_id.as_deref(),
            Some(parent_a.id.as_str())
        );

        // 无子父 → 空；不存在的父 → 空（纯查询原语，不校验父存在性）
        let parent_c = store.create_session(None, None, None).await.unwrap();
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
}
