//! Session 写操作面（创建 / 删除 / 元数据更新）
//!
//! sessions 表的写操作收敛为三个入口:
//! - **创建**:[`super::SessionStore::create_session`](构造 + 落库 + id 冲突重试一条龙)
//! - **更新**:[`super::SessionStore::update_session`](title / system_prompt 局部 UPDATE)
//! - **删除**:[`super::SessionStore::delete`](会话组级联删除)
//!
//! 只读查询面(get / list_all / list_child_sessions / count_with_filter)在
//! [`super::session_query`] 模块。
//!
//! **DB 唯一数据源**:session 的计数字段(message_count / tool_call_count /
//! total_* / total_cost)由 [`super::message`] 模块的 `insert_message` 事务内
//! SQL 原子自增维护,压缩元数据(compression_count / last_compacted_seq)由
//! [`super::compaction`] / [`super::rollback`] 的局部 UPDATE 维护。本模块只管
//! 「整行建」与「离散字段改」,不存在全量 `update`——避免单字段写
//! 被全量写覆盖(标题 bug 的根因)。
//!
//! 注:消息(Message)不在内存——产生即通过 [`super::SessionStore::insert_message`]
//! 单条落 DB,需要时按 session_id 查询(见 [`super::message`] 模块)。

use crate::error::SessionError;
use fuyao_api::Session;

/// session id 主键冲突时的最大重试次数（不含首次尝试）
///
/// 32 bit 熵下连续碰撞到这个次数的概率近乎零，命中即视为不可恢复故障向上抛错。
pub(super) const ID_CONFLICT_MAX_RETRIES: usize = 3;

/// 构造一个尚未落库的新会话（store 内部唯一构造点）
///
/// 生成 8 位 id、初始化 started_at / last_active_at 为当前时间、默认标题
/// 「新会话」。字段零值（计数 / 消费 / 压缩元数据 / child_count）由
/// `Session::default()` 承载。`create_session` 与 fork 类复制路径
/// （`fork_to` / `fork_visible`）共用本构造点。
pub(super) fn new_session(
    workspace: Option<String>,
    parent_session_id: Option<String>,
    system_prompt: Option<String>,
) -> Session {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    Session {
        id: generate_id(),
        title: Some("新会话".to_string()),
        system_prompt,
        started_at: now,
        last_active_at: now,
        parent_session_id,
        workspace,
        ..Session::default()
    }
}

/// 生成 8 位会话 id：取 UUID v4 第一段（8 个十六进制字符，32 bit 熵）
///
/// 个人单用户场景下碰撞概率可忽略；DB 主键约束兜底，碰撞时由
/// [`super::SessionStore::create_session`]（池上路径）与 `fork_to` / `fork_visible`
/// （事务内路径）重新生成重试。
pub(super) fn generate_id() -> String {
    uuid::Uuid::new_v4()
        .to_string()
        .split('-')
        .next()
        .unwrap_or("unknown")
        .to_string()
}

/// 校验 session 存在（事务内执行，复用调用方的事务连接）
///
/// 不存在返回 [`SessionError::NotFound`]。供 update_session（本模块）与
/// mark_compaction / rollback_to（兄弟模块）等写入路径复用——
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
    // ── 生命周期写操作（整行建 / 删）──────────────────────

    /// 创建会话（构造 + 落库一条龙，主键冲突自动重试）
    ///
    /// 内部完成：生成 8 位 id 与时间戳 → INSERT sessions 元数据行。id 与既有行
    /// 碰撞时（概率极低）捕获主键冲突错误，重新生成 id 再试，最多额外重试
    /// [`ID_CONFLICT_MAX_RETRIES`] 次。**只重试主键冲突**——其它错误（磁盘满、
    /// 连接断、schema 错误等）立即返回，重试无意义且会掩盖真实故障。
    ///
    /// 消息产生时由调用方经 `insert_message` 单条落库,不在此处批量写。
    ///
    /// # 参数
    /// - `workspace`:工作目录绝对路径（创建时定死;无工作目录传 `None`）
    /// - `parent_session_id`:父会话 id（`None` = 主会话;`Some` = 子任务会话）
    /// - `system_prompt`:系统提示词（可为 `None`）
    ///
    /// # 返回
    /// 落库后的完整会话（id 为最终值，与 DB 一致）。
    pub async fn create_session(
        &self,
        workspace: Option<String>,
        parent_session_id: Option<String>,
        system_prompt: Option<String>,
    ) -> Result<Session, SessionError> {
        let mut session = new_session(workspace, parent_session_id, system_prompt);
        for attempt in 0..=ID_CONFLICT_MAX_RETRIES {
            match Self::insert_session_row(&self.pool, &session).await {
                Ok(()) => {
                    tracing::info!(session_id = %session.id, "会话已创建");
                    return Ok(session);
                }
                Err(e) if e.is_primary_key_conflict() && attempt < ID_CONFLICT_MAX_RETRIES => {
                    tracing::warn!(
                        attempt = attempt + 1,
                        session_id = %session.id,
                        cause = "session id 主键冲突，重新生成 id 重试",
                    );
                    session.id = generate_id();
                }
                Err(e) => return Err(e),
            }
        }
        // 循环边界保证不会走到这里，循环条件 attempt < MAX 已在上一次迭代返回；
        // 此行仅为让编译器确认返回路径完备。
        unreachable!("重试循环已在边界内返回 Ok 或 Err")
    }

    /// 在指定执行器上插入一条 session 元数据行(建行写入体)
    ///
    /// 只负责写行本身,不含 id 冲突重试——重试策略在
    /// [`Self::create_session`](池上路径);事务内路径在
    /// [`fork_to`](super::SessionStore::fork_to)。16 列全量对齐 schema,
    /// 池连接与事务连接共用同一份 INSERT。
    pub(super) async fn insert_session_row<'e, E>(
        executor: E,
        session: &Session,
    ) -> Result<(), SessionError>
    where
        E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
    {
        sqlx::query(
            "INSERT INTO sessions (id, started_at,
                message_count, tool_call_count, total_prompt_tokens, total_completion_tokens,
                total_reasoning_tokens, total_cached_tokens, total_cost, title, system_prompt,
                compression_count, last_compacted_seq, parent_session_id, workspace, last_active_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )
        .bind(session.id.as_str())
        .bind(session.started_at)
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
        .execute(executor)
        .await?;

        Ok(())
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

    // ── 元数据局部更新 ─────────────────────────────────────────
    //
    // update_session 只 UPDATE sessions 表的字段,不动其他行、不动 messages 表。
    // 它服务于"只改元数据字段"的离散场景(标题异步生成 / 压缩后重建 prompt)。
    // session 的计数字段不在此处——由 insert_message 事务内原子自增维护;
    // 压缩元数据(compression_count / last_compacted_seq)由 compaction / rollback
    // 模块的局部 UPDATE 维护。DB 唯一数据源,无全量写,杜绝字段覆盖。

    /// 更新 session 的元数据字段（title / system_prompt，单事务局部 UPDATE）
    ///
    /// 两类消费场景共用一个入口：
    /// - 标题自动生成后异步落库（react 层 fire-and-forget spawn——spawn 的 future
    ///   是 `'static` 的，无法借用 `&mut Session`，故走局部 SQL 而非全量写）
    /// - 压缩后重建系统提示词落库（避免旧 prompt 中残留的动态内容在已被压进
    ///   摘要后误导模型）
    ///
    /// `title` / `system_prompt` 均传 `None` 时为无操作，直接返回 `Ok(())`。
    ///
    /// # 错误
    /// - [`SessionError::NotFound`]:session_id 在数据库中不存在
    pub async fn update_session(
        &self,
        session_id: &str,
        title: Option<&str>,
        system_prompt: Option<&str>,
    ) -> Result<(), SessionError> {
        if title.is_none() && system_prompt.is_none() {
            return Ok(());
        }

        let mut tx = self.pool.begin().await?;

        // 校验 session 存在(避免给不存在的 session 写脏数据)
        require_session(&mut tx, session_id).await?;

        sqlx::query(
            "UPDATE sessions SET
                title = COALESCE(?2, title),
                system_prompt = COALESCE(?3, system_prompt)
             WHERE id = ?1",
        )
        .bind(session_id)
        .bind(title)
        .bind(system_prompt)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        tracing::info!(
            session_id = session_id,
            title_updated = title.is_some(),
            system_prompt_updated = system_prompt.is_some(),
            "会话元数据已更新"
        );
        Ok(())
    }
}
