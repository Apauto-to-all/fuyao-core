//! SQLite 存储层
//!
//! 使用 sqlx（async）+ SqlitePool 连接池，原生 async，无需 spawn_blocking 包装。
//! SessionStore 是 session 持久化的唯一入口：全部 SQL（sessions / messages /
//! todos 三表）收敛在 store 模块内经其方法执行，连接池为私有字段不外泄。
//!
//! 模块组织（按职责分文件，各文件单一职责）：
//! - [`row`]：sessions / messages 表的行映射（DB 行 ↔ 领域类型）
//! - [`session`]：sessions 表的写操作面——创建（create_session，构造 + 落库 +
//!   id 冲突重试一条龙）/ 删除（delete 会话组级联）/ 元数据更新（update_session）
//! - [`session_query`]：sessions 表的只读查询面——get / list_all /
//!   list_child_sessions / count_with_filter
//! - [`sql`]：store 层共享 SQL 片段单点——messages 列清单 / fork 复制投影 /
//!   fork 聚合重算列段 / 可见窗口谓词，各路径同源引用
//! - [`message`]：messages 表的全部操作——写入（insert，事务内同时累加 sessions
//!   计数 / 费用）/ 计数（count_user_messages）/ 查询（load_full_history 全量审计 /
//!   list_messages_before 游标分页浏览）
//! - [`compaction`]：压缩边界写入（mark_compaction + CompressionReason，局部 UPDATE
//!   sessions 的 compression_count / last_compacted_seq）
//! - [`fork`]：对话派生（复制 seq < target 的消息到新独立会话，单事务：建行 +
//!   复制 + 按复制结果重算元数据；目标必须是 user / compaction 消息）
//! - [`fork_visible`]：可见窗口派生（整窗复制最新摘要起的可见上下文为新会话，
//!   parent 由调用方指定，单事务：建行 + 复制 + 按复制结果聚合计数）
//! - [`visible_window`]：给 LLM 的可见窗口查询（压缩感知，最新摘要 + 摘要后新消息）。
//!   与 [`message`] 的「给人看的」查询路径正交
//! - [`rollback`]：对话回退（删目标 seq 之后消息 + 局部 UPDATE 重算 count 类与
//!   压缩元数据，保护消费类字段不动）
//! - [`todo`]：todos 表的读写 + 级联删除（任务列表 CRUD）

pub(crate) mod compaction;
mod fork;
mod fork_visible;
mod message;
mod rollback;
mod row;
mod session;
mod session_query;
mod sql;
mod todo;
mod visible_window;

// 压缩原因持久层枚举对外导出（供消费方 fuyao-core 从事件层枚举转换后落库）
pub use compaction::CompressionReason;

use crate::error::SessionError;
use crate::schema::{SCHEMA_SQL, SCHEMA_VERSION};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::path::PathBuf;
use std::time::Duration;

/// 会话存储层
///
/// 持有 SqlitePool 连接池，提供 Session + Message 的 CRUD。
/// 连接池为私有字段，SQL 只经本类型的方法执行，不向其他模块暴露数据库句柄。
pub struct SessionStore {
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
            .busy_timeout(Duration::from_secs(storage.busy_timeout_secs))
            // 页缓存上限：负值语义为 KiB（-64000 = 每连接 64MB），页按需分配、不预占内存。
            // 历史会话反复读取的热页（可见窗口 / 分页浏览）留在进程内存，避免每次
            // 读写都穿透到系统页缓存之外；池内每条连接独立持有一份缓存。
            .pragma("cache_size", "-64000")
            // mmap 上限 1GB：把数据库文件以只读 mmap 映射进进程地址空间，读路径直接
            // 访问映射页，省去每次读取的 read 系统调用；按需缺页加载、不预占内存，
            // 与页缓存配合加速大库冷读。设上限而非全量映射，防超大库无界占用地址空间；
            // 写路径仍走页缓存正常回写，不受影响。
            .pragma("mmap_size", "1073741824");

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
        Ok(Self { pool })
    }

    /// 刷新查询计划统计
    ///
    /// 执行 `PRAGMA optimize`：让 SQLite 按需对部分表触发内部 ANALYZE，
    /// 更新索引列的数据分布统计，使复合索引的选择依据在长期运行后仍保持新鲜。
    /// 该操作幂等且通常耗时很小，适合在存储收尾（如引擎关闭）前调用一次。
    ///
    /// # 错误
    /// SQL 执行失败时返回 [`SessionError`]。
    pub async fn optimize(&self) -> Result<(), SessionError> {
        sqlx::query("PRAGMA optimize").execute(&self.pool).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造临时存储（隔离的临时目录）
    async fn temp_store() -> SessionStore {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let db_path = dir.path().join("test.db");
        std::mem::forget(dir);
        SessionStore::new(db_path).await.expect("创建存储失败")
    }

    #[tokio::test]
    async fn optimize_succeeds_on_fresh_store() {
        let store = temp_store().await;
        // 幂等操作：临时库上执行应成功（内部按需决定是否触发 ANALYZE）
        store.optimize().await.unwrap();
        // 重复调用同样成功（幂等性）
        store.optimize().await.unwrap();
    }
}
