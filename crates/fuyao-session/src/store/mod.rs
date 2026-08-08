//! SQLite 存储层
//!
//! 使用 sqlx（async）+ SqlitePool 连接池，原生 async，无需 spawn_blocking 包装。
//! SessionStore 是 session 持久化的唯一入口，持有连接池供外部（如引擎层）共享。
//!
//! 模块组织（按职责分文件，各文件单一职责）：
//! - [`row`]：sessions / messages 表的行映射（DB 行 ↔ 领域类型）
//! - [`session`]：sessions 表的全部操作——生命周期读写（create/get/delete/list）+
//!   单字段局部更新（update_system_prompt / update_title / end_session）
//! - [`message`]：messages 表的全部操作——写入（insert，事务内同时累加 sessions
//!   计数 / 费用）/ 计数（count）/ 查询（load_full_history 全量审计 /
//!   list_messages_before 游标分页浏览）
//! - [`compaction`]：压缩边界写入（mark_compaction + CompressionReason，局部 UPDATE
//!   sessions 的 compression_count / last_compacted_seq）
//! - [`visible_window`]：给 LLM 的可见窗口动态拼接（压缩感知，摘要 + keep_recent + 新消息）。
//!   与 [`message`] 的「给人看的」查询路径正交
//! - [`rollback`]：对话回退（删目标 seq 之后消息 + 局部 UPDATE 重算 count 类与
//!   压缩元数据，保护消费类字段不动）
//! - [`todo`]：todos 表的读写 + 级联删除（任务列表 CRUD）

pub(crate) mod compaction;
mod message;
mod rollback;
mod row;
mod session;
mod todo;
mod visible_window;

use crate::error::SessionError;
use crate::schema::{SCHEMA_SQL, SCHEMA_VERSION};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::path::PathBuf;
use std::time::Duration;

/// 会话存储层
///
/// 持有 SqlitePool 连接池，提供 Session + Message 的 CRUD。
/// 连接池可经 [`pool`](Self::pool) 对外共享，供引擎层或兄弟模块复用同一连接池。
pub struct SessionStore {
    db_path: PathBuf,
    pool: SqlitePool,
}

impl SessionStore {
    /// 创建并初始化存储
    ///
    /// 连接参数（busy_timeout / max_connections）从全局配置 `get_config().session.storage` 读取。
    pub async fn new(db_path: PathBuf) -> Result<Self, SessionError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let storage = fuyao_api::get_config().session.storage.clone();

        // 连接选项：WAL + 同步 Normal + 外键 + 忙等待
        // synchronous=Normal：WAL 模式下的推荐搭配，commit 不强制 fsync，
        // 写锁持有时间从 FULL 的几十毫秒降到亚毫秒级，从根上消除多连接并发写时的锁竞争。
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(storage.busy_timeout_secs));

        let pool = SqlitePoolOptions::new()
            .max_connections(storage.max_connections)
            .connect_with(options)
            .await?;

        // 初始化 schema
        sqlx::raw_sql(SCHEMA_SQL).execute(&pool).await?;

        // 写入 schema 版本（仅首次）
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schema_version")
            .fetch_one(&pool)
            .await?;
        if count == 0 {
            sqlx::query("INSERT INTO schema_version (version) VALUES (?1)")
                .bind(SCHEMA_VERSION)
                .execute(&pool)
                .await?;
        }

        tracing::info!(db_path = %db_path.display(), "会话存储初始化完成");
        Ok(Self { db_path, pool })
    }

    /// 数据库文件路径
    pub fn db_path(&self) -> &PathBuf {
        &self.db_path
    }

    /// 获取连接池引用（供引擎层或兄弟模块共享同一连接池）
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}
