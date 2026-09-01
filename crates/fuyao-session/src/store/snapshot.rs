//! file_snapshots 表：文件快照薄日志
//!
//! 每个工具批执行前记一行：本批执行前的基线树（`tree_hash`，影子仓 write-tree
//! 结果）与跟上一快照比的变更文件集（`files`，工作区相对路径）。行与消息以同一
//! `msg_seq` 关联（本工具批对应的 assistant 消息 seq），且以同一 `seq >= target`
//! 谓词同生共死——[`super::SessionStore::rollback_to`] 删消息的单事务内同步删快照行，
//! 会话删除的手动级联事务补删组内快照行。
//!
//! # seq 复用与行身份
//!
//! 消息 seq 由 `MAX(seq)+1` 分配、回退后会复用，但快照行随消息同谓词删除，复用的
//! seq 属于新快照——自增 id 主键（AUTOINCREMENT）保证行身份回退后也不复用，回退后
//! 新插入的行与历史行永不混淆。
//!
//! # files 列的 JSON 边界
//!
//! 变更文件清单以 JSON 数组落库；序列化 / 反序列化收敛在本模块，对外暴露
//! `Vec<String>`。反序列化失败按错误上抛（不静默丢弃——触碰集是回退删文件的依据）。

use crate::error::SessionError;

/// 快照行（查询产出类型：files 已从 JSON 解析为文件清单）
#[derive(Debug, Clone)]
pub struct FileSnapshotRow {
    /// 自增主键——行身份，回退后也不复用
    pub id: i64,
    /// 本工具批对应 assistant 消息的 seq
    pub msg_seq: i64,
    /// 本批执行前的基线树（影子仓 write-tree 结果）
    pub tree_hash: String,
    /// 跟上一快照比的变更文件（工作区相对路径）；会话首行为空集
    pub files: Vec<String>,
}

/// 把 files 列的 JSON 文本解析为文件清单（失败上抛，不降级）
fn parse_files(raw: &str) -> Result<Vec<String>, SessionError> {
    serde_json::from_str(raw).map_err(|e| SessionError::SnapshotFilesJson(e.to_string()))
}

impl super::SessionStore {
    /// 插入一条快照行（工具批执行前基线的薄日志）
    ///
    /// 快照触发方（ReAct 循环的工具批边界）在拿到基线树与变更文件集后落一行；
    /// `files` 在此层序列化为 JSON 数组入库，空集落 `'[]'`。
    ///
    /// # 参数
    /// - `session_id`:所属会话
    /// - `msg_seq`:本工具批对应 assistant 消息的 seq
    /// - `tree_hash`:本批执行前的基线树
    /// - `files`:跟上一快照比的变更文件（工作区相对路径）
    ///
    /// # 错误
    /// - [`SessionError::SqlxError`]:SQL 执行失败
    /// - [`SessionError::SnapshotFilesJson`]:files 序列化失败
    pub async fn insert_file_snapshot(
        &self,
        session_id: &str,
        msg_seq: i64,
        tree_hash: &str,
        files: &[String],
    ) -> Result<(), SessionError> {
        let files_json = serde_json::to_string(files)
            .map_err(|e| SessionError::SnapshotFilesJson(e.to_string()))?;
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        sqlx::query(
            "INSERT INTO file_snapshots (session_id, msg_seq, tree_hash, files, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(session_id)
        .bind(msg_seq)
        .bind(tree_hash)
        .bind(&files_json)
        .bind(created_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// 查询 `msg_seq >= from_seq` 的快照行（按 msg_seq 升序，同 seq 按 id 升序）
    ///
    /// 回退编排的输入：首行即最小 seq 的基线树，各行 `files` 的并集即触碰集。
    /// 无匹配行返回空 Vec（回退目标后无工具批的零成本路径）。
    ///
    /// # 错误
    /// - [`SessionError::SqlxError`]:SQL 执行失败
    /// - [`SessionError::SnapshotFilesJson`]:某行 files 列 JSON 解析失败（脏数据）
    pub async fn list_file_snapshots_from(
        &self,
        session_id: &str,
        from_seq: i64,
    ) -> Result<Vec<FileSnapshotRow>, SessionError> {
        let rows: Vec<(i64, i64, String, String)> = sqlx::query_as(
            "SELECT id, msg_seq, tree_hash, files FROM file_snapshots
             WHERE session_id = ?1 AND msg_seq >= ?2
             ORDER BY msg_seq ASC, id ASC",
        )
        .bind(session_id)
        .bind(from_seq)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|(id, msg_seq, tree_hash, files_raw)| {
                Ok(FileSnapshotRow {
                    id,
                    msg_seq,
                    tree_hash,
                    files: parse_files(&files_raw)?,
                })
            })
            .collect()
    }

    /// 查询本会话最新一条快照行的基线树
    ///
    /// 快照触发方（ReAct 循环的工具批边界）做增量拍时取 prev_tree 用：只取
    /// tree_hash 单列（不解析各行 files JSON），是 [`Self::list_file_snapshots_from`]
    /// 的轻量尾部查询。会话无快照行返回 `None`（增量拍的「首拍」语义）。
    ///
    /// # 错误
    /// - [`SessionError::SqlxError`]:SQL 执行失败
    pub async fn latest_file_snapshot_tree(
        &self,
        session_id: &str,
    ) -> Result<Option<String>, SessionError> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT tree_hash FROM file_snapshots WHERE session_id = ?1
             ORDER BY msg_seq DESC, id DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(tree_hash,)| tree_hash))
    }

    /// 删除 `msg_seq >= from_seq` 的快照行，返回删到的行数
    ///
    /// 独立删除入口（池上路径）；`rollback_to` 的单事务联动走
    /// [`Self::delete_file_snapshots_from_in_tx`]。
    ///
    /// # 错误
    /// - [`SessionError::SqlxError`]:SQL 执行失败
    pub async fn delete_file_snapshots_from(
        &self,
        session_id: &str,
        from_seq: i64,
    ) -> Result<u64, SessionError> {
        let result =
            sqlx::query("DELETE FROM file_snapshots WHERE session_id = ?1 AND msg_seq >= ?2")
                .bind(session_id)
                .bind(from_seq)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected())
    }

    /// 事务内删除 `msg_seq >= from_seq` 的快照行，返回删到的行数
    ///
    /// 供 [`rollback_to`](super::SessionStore::rollback_to) 在删消息的同一事务内
    /// 调用——消息与快照以同一 `seq >= target` 谓词同生共死，无独立提交窗口。
    pub(super) async fn delete_file_snapshots_from_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        session_id: &str,
        from_seq: i64,
    ) -> Result<u64, SessionError> {
        let result =
            sqlx::query("DELETE FROM file_snapshots WHERE session_id = ?1 AND msg_seq >= ?2")
                .bind(session_id)
                .bind(from_seq)
                .execute(&mut **tx)
                .await?;
        Ok(result.rows_affected())
    }

    /// 事务内删除会话组的全部快照行（会话删除级联清理用）
    ///
    /// 清理范围为「根会话 + 其全部子会话」的会话组；组 id 集合经子查询取自
    /// sessions 表，行仍在时子查询才取得到。仅供 [`delete`](super::SessionStore::delete)
    /// 在事务内调用，保证删会话时快照账无残留。
    pub(super) async fn delete_file_snapshots_group_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        session_id: &str,
    ) -> Result<(), SessionError> {
        sqlx::query(
            "DELETE FROM file_snapshots WHERE session_id IN
             (SELECT id FROM sessions WHERE id = ?1 OR parent_session_id = ?1)",
        )
        .bind(session_id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::SessionStore;
    use crate::error::SessionError;
    use fuyao_api::Message;

    /// 构造临时存储（隔离的临时目录）
    async fn temp_store() -> SessionStore {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let db_path = dir.path().join("test.db");
        std::mem::forget(dir);
        SessionStore::new(db_path).await.expect("创建存储失败")
    }

    /// 插入一条 user 消息，返回其 seq（rollback_to 的合法目标）
    async fn insert_user(store: &SessionStore, sid: &str, content: &str) -> i64 {
        let mut msg = Message::user(content.to_string());
        store.insert_message(sid, &mut msg).await.unwrap()
    }

    /// 插入一条 assistant 消息，返回其 seq（快照行的 msg_seq 关联点）
    async fn insert_assistant(store: &SessionStore, sid: &str, content: &str) -> i64 {
        let mut msg = Message::assistant(Some(content.to_string()));
        store.insert_message(sid, &mut msg).await.unwrap()
    }

    /// 插入一条快照行
    async fn insert_snap(store: &SessionStore, sid: &str, msg_seq: i64, tree: &str) {
        store
            .insert_file_snapshot(
                sid,
                msg_seq,
                tree,
                &[format!("a/{msg_seq}.rs"), "目录/文件 b.txt".to_string()],
            )
            .await
            .unwrap();
    }

    // ===== 表 CRUD =====

    #[tokio::test]
    async fn insert_then_list_roundtrips_files_json() {
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();

        insert_snap(&store, &session.id, 2, "tree_a").await;
        insert_snap(&store, &session.id, 4, "tree_b").await;
        // 会话首行的空集路径：files 落 '[]'，读回应为空 Vec
        store
            .insert_file_snapshot(&session.id, 5, "tree_c", &[])
            .await
            .unwrap();

        // 全量查（from 0）：按 msg_seq 升序，files 解析回 Vec<String>（含中文路径）
        let rows = store
            .list_file_snapshots_from(&session.id, 0)
            .await
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].msg_seq, 2);
        assert_eq!(rows[0].tree_hash, "tree_a");
        assert_eq!(rows[0].files, vec!["a/2.rs", "目录/文件 b.txt"]);
        assert_eq!(rows[1].msg_seq, 4);
        assert_eq!(rows[2].msg_seq, 5);
        assert!(rows[2].files.is_empty(), "空 files 应往返为空 Vec");

        // 谓词查（from 4）：只含 msg_seq >= 4 的两行
        let tail = store
            .list_file_snapshots_from(&session.id, 4)
            .await
            .unwrap();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].msg_seq, 4);
        assert_eq!(tail[1].msg_seq, 5);

        // 谓词查无匹配：空 Vec
        let none = store
            .list_file_snapshots_from(&session.id, 99)
            .await
            .unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn list_is_isolated_by_session_id() {
        let store = temp_store().await;
        let sa = store.create_session(None, None, None).await.unwrap();
        let sb = store.create_session(None, None, None).await.unwrap();

        insert_snap(&store, &sa.id, 2, "tree_a").await;
        insert_snap(&store, &sb.id, 2, "tree_b").await;

        let ra = store.list_file_snapshots_from(&sa.id, 0).await.unwrap();
        assert_eq!(ra.len(), 1);
        assert_eq!(ra[0].tree_hash, "tree_a");
        let rb = store.list_file_snapshots_from(&sb.id, 0).await.unwrap();
        assert_eq!(rb.len(), 1);
        assert_eq!(rb[0].tree_hash, "tree_b");
    }

    #[tokio::test]
    async fn delete_from_seq_removes_only_matching_rows() {
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();
        insert_snap(&store, &session.id, 2, "tree_a").await;
        insert_snap(&store, &session.id, 4, "tree_b").await;
        insert_snap(&store, &session.id, 5, "tree_c").await;

        // 删 msg_seq >= 4：命中 2 行，msg_seq=2 的行保留
        let deleted = store
            .delete_file_snapshots_from(&session.id, 4)
            .await
            .unwrap();
        assert_eq!(deleted, 2);
        let rows = store
            .list_file_snapshots_from(&session.id, 0)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].msg_seq, 2);

        // 无匹配时删除 0 行
        let deleted = store
            .delete_file_snapshots_from(&session.id, 99)
            .await
            .unwrap();
        assert_eq!(deleted, 0);
    }

    #[tokio::test]
    async fn corrupt_files_json_fails_the_read() {
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();

        // 直接落一行脏 JSON（绕过 insert 的序列化边界，构造历史脏数据场景）
        sqlx::query("INSERT INTO file_snapshots (session_id, msg_seq, tree_hash, files, created_at) VALUES (?1, 2, 'tree', 'not-json', 0.0)")
            .bind(&session.id)
            .execute(&store.pool)
            .await
            .unwrap();

        let result = store.list_file_snapshots_from(&session.id, 0).await;
        assert!(
            matches!(result, Err(SessionError::SnapshotFilesJson(_))),
            "脏 files JSON 应按错误上抛，不静默丢弃"
        );
    }

    /// 尾部轻量查询：取本会话最新快照行的基线树（增量拍 prev_tree）
    #[tokio::test]
    async fn latest_file_snapshot_tree_returns_tail_and_none() {
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();

        // 无快照行：None（首拍语义）
        assert_eq!(
            store.latest_file_snapshot_tree(&session.id).await.unwrap(),
            None
        );

        insert_snap(&store, &session.id, 2, "tree_a").await;
        insert_snap(&store, &session.id, 4, "tree_b").await;

        // 多行时取 msg_seq 最大者
        assert_eq!(
            store.latest_file_snapshot_tree(&session.id).await.unwrap(),
            Some("tree_b".to_string())
        );

        // 会话间隔离：他会话不串
        let other = store.create_session(None, None, None).await.unwrap();
        assert_eq!(
            store.latest_file_snapshot_tree(&other.id).await.unwrap(),
            None
        );
    }

    /// 同 msg_seq 多行时按 id 取最新插入的一行
    #[tokio::test]
    async fn latest_file_snapshot_tree_breaks_ties_by_id() {
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();
        // 直接落两行同 msg_seq 的快照（seq 复用场景之外的重复行）
        store
            .insert_file_snapshot(&session.id, 3, "tree_first", &[])
            .await
            .unwrap();
        store
            .insert_file_snapshot(&session.id, 3, "tree_second", &[])
            .await
            .unwrap();
        assert_eq!(
            store.latest_file_snapshot_tree(&session.id).await.unwrap(),
            Some("tree_second".to_string())
        );
    }

    // ===== rollback_to 联动删行 =====

    #[tokio::test]
    async fn rollback_to_deletes_snapshots_with_same_predicate() {
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();

        insert_user(&store, &session.id, "u1").await; // seq 1
        let a1 = insert_assistant(&store, &session.id, "a1").await; // seq 2
        insert_snap(&store, &session.id, a1, "tree_a").await;
        let u2 = insert_user(&store, &session.id, "u2").await; // seq 3
        let a2 = insert_assistant(&store, &session.id, "a2").await; // seq 4
        insert_snap(&store, &session.id, a2, "tree_b").await;

        // 回退到 u2：删 seq >= 3 的消息，同谓词删 msg_seq >= 3 的快照行
        store.rollback_to(&session.id, u2).await.unwrap();

        let rows = store
            .list_file_snapshots_from(&session.id, 0)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "只剩 msg_seq=2 的快照行");
        assert_eq!(rows[0].msg_seq, a1);
        assert_eq!(rows[0].tree_hash, "tree_a");

        // 消息侧同步删除（联动不改既有消息回退语义）
        let full = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(full.len(), 2);
    }

    #[tokio::test]
    async fn rollback_failure_keeps_snapshot_rows() {
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();

        insert_user(&store, &session.id, "u1").await; // seq 1
        let a1 = insert_assistant(&store, &session.id, "a1").await; // seq 2
        insert_snap(&store, &session.id, a1, "tree_a").await;

        // assistant 中间态不可作为回退目标：事务整体失败，快照行不动
        let result = store.rollback_to(&session.id, a1).await;
        assert!(matches!(result, Err(SessionError::InvalidCutTarget(_))));

        let rows = store
            .list_file_snapshots_from(&session.id, 0)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "校验失败不应动快照账");
    }

    // ===== seq 复用：回退后新插行与历史不混淆 =====

    #[tokio::test]
    async fn seq_reuse_after_rollback_never_mixes_rows() {
        let store = temp_store().await;
        let session = store.create_session(None, None, None).await.unwrap();

        insert_user(&store, &session.id, "u1").await; // seq 1
        let a1 = insert_assistant(&store, &session.id, "a1").await; // seq 2
        insert_snap(&store, &session.id, a1, "tree_a").await;
        let u2 = insert_user(&store, &session.id, "u2").await; // seq 3
        let a2 = insert_assistant(&store, &session.id, "a2").await; // seq 4
        insert_snap(&store, &session.id, a2, "tree_old").await;

        // 记下回退前 msg_seq=4 行的 id（历史行身份）
        let before = store
            .list_file_snapshots_from(&session.id, 0)
            .await
            .unwrap();
        let old_id = before.iter().find(|r| r.msg_seq == 4).unwrap().id;

        // 回退到 u2：删 seq >= 3 的消息与快照行；后续消息 seq 从 3 起复用
        store.rollback_to(&session.id, u2).await.unwrap();

        let u2b = insert_user(&store, &session.id, "u2'").await; // seq 3（复用）
        let a2b = insert_assistant(&store, &session.id, "a2'").await; // seq 4（复用）
        assert_eq!(u2b, 3);
        assert_eq!(a2b, 4);

        // 复用 seq 上插入新快照：id 必须是全新值，与历史行不混淆
        insert_snap(&store, &session.id, a2b, "tree_new").await;

        let after = store
            .list_file_snapshots_from(&session.id, 0)
            .await
            .unwrap();
        assert_eq!(after.len(), 2, "历史行 + 新行");
        let new_row = after.iter().find(|r| r.msg_seq == 4).unwrap();
        assert_eq!(new_row.tree_hash, "tree_new");
        assert_ne!(new_row.id, old_id, "自增 id 行身份：回退后不复用");
    }

    // ===== 会话删除级联 =====

    #[tokio::test]
    async fn delete_session_cascades_snapshot_rows_for_group() {
        let store = temp_store().await;
        let parent = store.create_session(None, None, None).await.unwrap();
        let child = store
            .create_session(None, Some(parent.id.clone()), None)
            .await
            .unwrap();
        let outsider = store.create_session(None, None, None).await.unwrap();

        insert_snap(&store, &parent.id, 2, "tree_p").await;
        insert_snap(&store, &child.id, 2, "tree_c").await;
        insert_snap(&store, &outsider.id, 2, "tree_o").await;

        let deleted = store.delete(&parent.id).await.unwrap();
        assert!(deleted, "主会话行删到");

        // 组内（父 + 子）快照行无残留
        assert!(
            store
                .list_file_snapshots_from(&parent.id, 0)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .list_file_snapshots_from(&child.id, 0)
                .await
                .unwrap()
                .is_empty()
        );

        // 组外会话的快照行不受影响
        let rows = store
            .list_file_snapshots_from(&outsider.id, 0)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tree_hash, "tree_o");
    }
}
