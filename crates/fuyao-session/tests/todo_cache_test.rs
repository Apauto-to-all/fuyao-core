//! fuyao-session 集成测试：TodoStore + SessionManager 全局缓存隔离
//!
//! 聚焦两个独立关注点：
//! - TodoStore：read/write 全量替换语义（write 后 read 回读）
//! - SESSION_MANAGER_CACHE 全局缓存隔离：clear_session_manager_cache + 唯一路径 key

mod common;

use fuyao_api::TodoItem;
use fuyao_session::{clear_session_manager_cache, get_session_manager};
use std::sync::Arc;

// ============================================================================
// TodoStore：read/write 语义
// ============================================================================

#[tokio::test]
async fn todo_store_write_then_read_roundtrip() {
    let mgr = common::temp_manager().await;
    let session = mgr.create(None, None).await.unwrap();
    let todo_store = mgr.get_todo_store();

    let todos = vec![
        TodoItem {
            id: "1".to_string(),
            content: "任务一".to_string(),
            status: "pending".to_string(),
        },
        TodoItem {
            id: "2".to_string(),
            content: "任务二".to_string(),
            status: "completed".to_string(),
        },
    ];
    let written = todo_store
        .write(&session.id, todos.clone())
        .await
        .expect("写入应成功");
    assert_eq!(written.len(), 2);

    let read = todo_store.read(&session.id).await.expect("读取应成功");
    assert_eq!(read.len(), 2);
    assert_eq!(read[0].id, "1");
    assert_eq!(read[1].status, "completed");
}

#[tokio::test]
async fn todo_store_write_replaces_all() {
    // write 是全量替换（DELETE 后重插）
    let mgr = common::temp_manager().await;
    let session = mgr.create(None, None).await.unwrap();
    let todo_store = mgr.get_todo_store();

    // 第一次写 2 条
    todo_store
        .write(
            &session.id,
            vec![
                TodoItem {
                    id: "1".to_string(),
                    content: "旧".to_string(),
                    status: "pending".to_string(),
                },
                TodoItem {
                    id: "2".to_string(),
                    content: "旧".to_string(),
                    status: "pending".to_string(),
                },
            ],
        )
        .await
        .unwrap();
    // 第二次写 1 条（替换全部）
    todo_store
        .write(
            &session.id,
            vec![TodoItem {
                id: "3".to_string(),
                content: "新".to_string(),
                status: "in_progress".to_string(),
            }],
        )
        .await
        .unwrap();

    let read = todo_store.read(&session.id).await.unwrap();
    assert_eq!(read.len(), 1, "write 应全量替换");
    assert_eq!(read[0].id, "3");
}

#[tokio::test]
async fn todo_store_read_empty_session_returns_empty() {
    let mgr = common::temp_manager().await;
    let session = mgr.create(None, None).await.unwrap();
    let todo_store = mgr.get_todo_store();

    let read = todo_store.read(&session.id).await.unwrap();

    assert!(read.is_empty(), "未写入过的 session 应返回空列表");
}

#[tokio::test]
async fn todo_store_write_empty_clears_all() {
    let mgr = common::temp_manager().await;
    let session = mgr.create(None, None).await.unwrap();
    let todo_store = mgr.get_todo_store();

    todo_store
        .write(
            &session.id,
            vec![TodoItem {
                id: "1".to_string(),
                content: "x".to_string(),
                status: "pending".to_string(),
            }],
        )
        .await
        .unwrap();
    // 写空 Vec 清空
    todo_store.write(&session.id, vec![]).await.unwrap();

    let read = todo_store.read(&session.id).await.unwrap();
    assert!(read.is_empty(), "写空 Vec 应清空所有 todo");
}

#[tokio::test]
async fn todo_store_isolates_per_session() {
    // 不同 session 的 todo 互不干扰
    let mgr = common::temp_manager().await;
    let s1 = mgr.create(None, None).await.unwrap();
    let s2 = mgr.create(None, None).await.unwrap();
    let todo_store = mgr.get_todo_store();

    todo_store
        .write(
            &s1.id,
            vec![TodoItem {
                id: "1".to_string(),
                content: "s1".to_string(),
                status: "pending".to_string(),
            }],
        )
        .await
        .unwrap();

    let s2_read = todo_store.read(&s2.id).await.unwrap();
    assert!(s2_read.is_empty(), "s2 不应看到 s1 的 todo");
}

// ============================================================================
// SESSION_MANAGER_CACHE 全局缓存隔离
// ============================================================================

#[tokio::test]
async fn get_session_manager_caches_by_db_path() {
    // 同一 db_path 第二次调用返回缓存的同一 Arc
    let td = common::temp_db();
    let path = td.db_path.clone();

    let m1 = get_session_manager(path.clone()).await.unwrap();
    let m2 = get_session_manager(path.clone()).await.unwrap();

    assert!(
        Arc::ptr_eq(&m1, &m2),
        "同一 db_path 应返回缓存的同一 Arc<SessionManager>"
    );

    clear_session_manager_cache();
}

#[tokio::test]
async fn clear_cache_forces_recreation() {
    // clear 后再 get 返回新实例（不与旧的 ptr_eq）
    let td = common::temp_db();
    let path = td.db_path.clone();

    let m1 = get_session_manager(path.clone()).await.unwrap();
    clear_session_manager_cache();
    let m2 = get_session_manager(path.clone()).await.unwrap();

    assert!(!Arc::ptr_eq(&m1, &m2), "clear 后应返回新实例");

    clear_session_manager_cache();
}

#[tokio::test]
async fn different_db_paths_return_different_managers() {
    let td1 = common::temp_db();
    let td2 = common::temp_db();

    let m1 = get_session_manager(td1.db_path.clone()).await.unwrap();
    let m2 = get_session_manager(td2.db_path.clone()).await.unwrap();

    assert!(!Arc::ptr_eq(&m1, &m2), "不同 db_path 应返回不同实例");

    clear_session_manager_cache();
}

#[tokio::test]
async fn cached_manager_persists_data_across_lookups() {
    // 缓存的 manager 在多次 get 间保持数据（同一物理 DB）
    let td = common::temp_db();
    let path = td.db_path.clone();

    let m1 = get_session_manager(path.clone()).await.unwrap();
    let session = m1
        .create(Some("持久化测试".to_string()), None)
        .await
        .unwrap();

    // 第二次 get（命中缓存）
    let m2 = get_session_manager(path.clone()).await.unwrap();
    let loaded = m2.get(&session.id).await.unwrap().unwrap();

    assert_eq!(loaded.title.as_deref(), Some("持久化测试"));

    clear_session_manager_cache();
}
