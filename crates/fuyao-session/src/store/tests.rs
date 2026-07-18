//! SessionStore 单元测试（CRUD + 压缩血统链）

use super::SessionStore;
use crate::error::SessionError;
use fuyao_api::{Message, Session};
use tempfile::tempdir;

/// 构造临时存储（隔离的临时目录，测试结束自动清理）
async fn temp_store() -> SessionStore {
    let dir = tempdir().expect("创建临时目录失败");
    let db_path = dir.path().join("test.db");
    // 需要 leak 保活：tempdir 的 TempDir drop 时会删除目录，
    // 但 async 测试里 SessionStore 跨 await 持有路径，dir 必须存活到测试结束。
    // 这里用 forget 让目录留到进程结束（测试进程短生命周期，可接受）。
    std::mem::forget(dir);
    SessionStore::new(db_path).await.expect("创建存储失败")
}

#[tokio::test]
async fn store_create_and_get() {
    let store = temp_store().await;
    let mut session = Session::new(Some("测试".to_string()), None);
    session.messages.push(Message::user("你好".to_string()));

    store.create(&session).await.unwrap();
    let loaded = store.get(&session.id).await.unwrap().unwrap();
    assert_eq!(loaded.id, session.id);
    assert_eq!(loaded.title, Some("测试".to_string()));
    assert_eq!(loaded.messages.len(), 1);
    assert_eq!(loaded.messages[0].content, Some("你好".to_string()));
}

#[tokio::test]
async fn store_get_returns_none_for_missing() {
    let store = temp_store().await;
    let result = store.get("nonexistent").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn store_update_increments_messages() {
    let store = temp_store().await;
    let mut session = Session::new(None, None);
    session.messages.push(Message::user("第一条".to_string()));
    store.create(&session).await.unwrap();

    session
        .messages
        .push(Message::assistant(Some("回复".to_string())));
    session.message_count = 2;
    store.update(&session).await.unwrap();

    let loaded = store.get(&session.id).await.unwrap().unwrap();
    assert_eq!(loaded.messages.len(), 2);
    assert_eq!(loaded.messages[1].role, "assistant");
}

#[tokio::test]
async fn store_delete_removes_session() {
    let store = temp_store().await;
    let session = Session::new(None, None);
    store.create(&session).await.unwrap();

    let deleted = store.delete(&session.id).await.unwrap();
    assert!(deleted);

    assert!(store.get(&session.id).await.unwrap().is_none());
}

#[tokio::test]
async fn store_list_all_returns_sessions() {
    let store = temp_store().await;
    let s1 = Session::new(Some("会话1".to_string()), None);
    let s2 = Session::new(Some("会话2".to_string()), None);
    store.create(&s1).await.unwrap();
    store.create(&s2).await.unwrap();

    let list = store.list_all(10, 0).await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].title, Some("会话2".to_string()));
}

#[tokio::test]
async fn store_count_returns_correct_count() {
    let store = temp_store().await;
    assert_eq!(store.count().await.unwrap(), 0);
    store.create(&Session::new(None, None)).await.unwrap();
    assert_eq!(store.count().await.unwrap(), 1);
}

#[tokio::test]
async fn store_incremental_save_only_inserts_new() {
    let store = temp_store().await;
    let mut session = Session::new(None, None);
    session.messages.push(Message::user("m1".to_string()));
    session.messages.push(Message::user("m2".to_string()));
    store.create(&session).await.unwrap();

    session.messages.push(Message::user("m3".to_string()));
    store.update(&session).await.unwrap();

    let loaded = store.get(&session.id).await.unwrap().unwrap();
    assert_eq!(loaded.messages.len(), 3);
    assert_eq!(loaded.messages[2].content, Some("m3".to_string()));
}

#[tokio::test]
async fn store_tool_calls_serialized_as_json() {
    let store = temp_store().await;
    let mut session = Session::new(None, None);
    let mut msg = Message::assistant(None);
    msg.tool_calls = Some(serde_json::json!([{
        "id": "call_1",
        "type": "function",
        "function": { "name": "bash", "arguments": "{}" }
    }]));
    session.messages.push(msg);
    store.create(&session).await.unwrap();

    let loaded = store.get(&session.id).await.unwrap().unwrap();
    assert!(loaded.messages[0].tool_calls.is_some());
    assert_eq!(
        loaded.messages[0].tool_calls.as_ref().unwrap()[0]["id"],
        "call_1"
    );
}

// ===== 会话分裂（压缩血统链）测试 =====

#[tokio::test]
async fn split_session_creates_child_with_parent_link() {
    let store = temp_store().await;
    let parent = Session::new(None, None);
    store.create(&parent).await.unwrap();

    let child = store
        .split_session(
            &parent.id,
            None,
            vec![Message::system("压缩摘要".to_string())],
            None,
        )
        .await
        .unwrap();

    // 新会话的 parent_session_id 必须指向 parent（血统链建立）
    assert_eq!(child.parent_session_id, Some(parent.id.clone()));
    // 压缩消息完整继承
    assert_eq!(child.messages.len(), 1);
    assert_eq!(child.messages[0].content, Some("压缩摘要".to_string()));
    assert_eq!(child.message_count, 1);
}

#[tokio::test]
async fn split_session_marks_parent_as_compressed() {
    let store = temp_store().await;
    let parent = Session::new(None, None);
    store.create(&parent).await.unwrap();

    // 分裂前：parent 处于活跃状态
    assert!(parent.ended_at.is_none());
    assert!(parent.end_reason.is_none());

    store
        .split_session(&parent.id, None, vec![], None)
        .await
        .unwrap();

    // 分裂后：parent 标记为已结束（end_reason='compression'）
    let updated_parent = store.get(&parent.id).await.unwrap().unwrap();
    assert_eq!(updated_parent.end_reason, Some("compression".to_string()));
    assert!(updated_parent.ended_at.is_some());
}

#[tokio::test]
async fn split_session_returns_not_found_for_missing_parent() {
    let store = temp_store().await;
    let result = store.split_session("nonexistent", None, vec![], None).await;

    // parent 不存在必须报错（避免创建孤儿 child）
    assert!(matches!(result, Err(SessionError::NotFound(_))));
}

#[tokio::test]
async fn split_session_accumulates_token_stats_from_messages() {
    let store = temp_store().await;
    let parent = Session::new(None, None);
    store.create(&parent).await.unwrap();

    // 构造带 token 统计的压缩消息
    let mut summary = Message::assistant(Some("摘要内容".to_string()));
    summary.prompt_tokens = 100;
    summary.completion_tokens = 50;
    summary.reasoning_tokens = 20;
    summary.cached_tokens = 10;
    summary.cost = 0.005;

    let child = store
        .split_session(&parent.id, None, vec![summary], None)
        .await
        .unwrap();

    // session 级统计应从消息累加（保持一致）
    assert_eq!(child.total_prompt_tokens, 100);
    assert_eq!(child.total_completion_tokens, 50);
    assert_eq!(child.total_reasoning_tokens, 20);
    assert_eq!(child.total_cached_tokens, 10);
    assert!((child.total_cost - 0.005).abs() < f64::EPSILON);
    assert_eq!(child.message_count, 1);
}

#[tokio::test]
async fn resolve_current_returns_root_when_no_split() {
    // 未分裂过的会话：末端就是自己
    let store = temp_store().await;
    let session = Session::new(None, None);
    store.create(&session).await.unwrap();

    let current = store.resolve_current(&session.id).await.unwrap();
    assert_eq!(current, Some(session.id.clone()));
}

#[tokio::test]
async fn resolve_current_returns_leaf_after_single_split() {
    let store = temp_store().await;
    let parent = Session::new(None, None);
    store.create(&parent).await.unwrap();

    let child = store
        .split_session(&parent.id, None, vec![], None)
        .await
        .unwrap();

    // 从 parent 的 id 解析，应返回 child 的 id（末端）
    let current = store.resolve_current(&parent.id).await.unwrap();
    assert_eq!(current, Some(child.id.clone()));
    assert_ne!(current, Some(parent.id.clone()));
}

#[tokio::test]
async fn resolve_current_returns_last_leaf_after_multi_splits() {
    // 三代分裂：root → child1 → child2，从 root 解析应得到 child2
    let store = temp_store().await;
    let root = Session::new(None, None);
    store.create(&root).await.unwrap();

    let child1 = store
        .split_session(&root.id, None, vec![], None)
        .await
        .unwrap();
    // 加微小延迟确保 started_at 单调递增（resolve_current 按 started_at DESC 取最新）
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let child2 = store
        .split_session(&child1.id, None, vec![], None)
        .await
        .unwrap();

    let current = store.resolve_current(&root.id).await.unwrap();
    assert_eq!(current, Some(child2.id.clone()));
    assert_ne!(current, Some(child1.id.clone()));
    assert_ne!(current, Some(root.id.clone()));
}

#[tokio::test]
async fn resolve_current_returns_none_for_missing_root() {
    let store = temp_store().await;
    let current = store.resolve_current("nonexistent").await.unwrap();
    assert_eq!(current, None);
}

#[tokio::test]
async fn get_current_returns_leaf_session_with_messages() {
    let store = temp_store().await;
    let root = Session::new(None, None);
    store.create(&root).await.unwrap();

    let child = store
        .split_session(
            &root.id,
            Some("新系统提示".to_string()),
            vec![Message::system("压缩摘要".to_string())],
            Some("压缩会话".to_string()),
        )
        .await
        .unwrap();

    // 从 root id 取当前会话：应返回 child（含完整数据）
    let current = store.get_current(&root.id).await.unwrap().unwrap();
    assert_eq!(current.id, child.id);
    assert_eq!(current.system_prompt, Some("新系统提示".to_string()));
    assert_eq!(current.title, Some("压缩会话".to_string()));
    assert_eq!(current.messages.len(), 1);
    assert_eq!(current.parent_session_id, Some(root.id.clone()));
}

#[tokio::test]
async fn get_current_returns_none_for_missing_root() {
    let store = temp_store().await;
    let result = store.get_current("nonexistent").await.unwrap();
    assert!(result.is_none());
}
