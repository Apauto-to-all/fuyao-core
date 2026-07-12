//! fuyao-session 集成测试共享 fixture
//!
//! 所有 DB 路径基于 tempfile::TempDir 的唯一子目录，既隔离 SQLiteStore，
//! 也隔离 SESSION_MANAGER_CACHE 的 key（按 db_path 字符串缓存）。
//! 不调用 set_config，走 get_config 返回 default 的兜底。

// 跨测试二进制共享：未用部分不报 dead_code
#![allow(dead_code)]

use fuyao_session::{SQLiteStore, SessionManager};
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

/// 临时 DB 目录（含 TempDir 句柄 + db_path）
pub struct TempDb {
    /// 持有句柄，drop 时自动清理
    pub _dir: TempDir,
    /// sessions.db 完整路径
    pub db_path: PathBuf,
}

/// 创建临时 DB 目录：{tempdir}/{uuid}/test.db
///
/// 每次调用产生唯一路径，天然隔离 SESSION_MANAGER_CACHE key。
pub fn temp_db() -> TempDb {
    let dir = tempfile::tempdir().expect("创建临时目录失败");
    let db_path = dir.path().join("test.db");
    TempDb { _dir: dir, db_path }
}

/// 构造独立的 SQLiteStore（不经过全局缓存）
pub async fn temp_store() -> SQLiteStore {
    let td = temp_db();
    // SQLiteStore::new 会 create_dir_all(parent)
    SQLiteStore::new(td.db_path.clone())
        .await
        .expect("构造 SQLiteStore 失败")
}

/// 构造独立的 SessionManager（直接 new，不走 get_session_manager 全局缓存）
pub async fn temp_manager() -> Arc<SessionManager> {
    let store = temp_store().await;
    Arc::new(SessionManager::new(store))
}

/// 构造可注入 fuyao_home 的 AgentPaths（用于 add_message 的费用计算）
pub fn temp_agent_paths(fuyao_home: PathBuf) -> fuyao_api::AgentPaths {
    fuyao_api::AgentPaths {
        agent_id: None,
        workspace: None,
        extra_dirs: Vec::new(),
        fuyao_home,
    }
}
