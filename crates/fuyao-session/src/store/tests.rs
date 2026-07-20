//! SessionStore 单元测试（CRUD + 上下文压缩边界）

use super::SessionStore;
use super::compaction::CompressionReason;
use crate::error::SessionError;
use fuyao_api::{Message, MessageKind, Session};
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

// ===== 基础 CRUD 测试（适配新字段：seq / kind） =====

#[tokio::test]
async fn store_create_and_get() {
    let store = temp_store().await;
    let mut session = Session::new(Some("测试".to_string()), None);
    session.messages.push(Message::user("你好".to_string()));

    store.create(&mut session).await.unwrap();
    let loaded = store.get(&session.id).await.unwrap().unwrap();
    assert_eq!(loaded.id, session.id);
    assert_eq!(loaded.title, Some("测试".to_string()));
    assert_eq!(loaded.messages.len(), 1);
    assert_eq!(loaded.messages[0].content, Some("你好".to_string()));
    // 新会话从未压缩：compression_count=0, last_compacted_seq=None
    assert_eq!(loaded.compression_count, 0);
    assert!(loaded.last_compacted_seq.is_none());
    // 首条消息 seq=1
    assert_eq!(loaded.messages[0].seq, 1);
    assert_eq!(loaded.messages[0].kind, MessageKind::Message);
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
    store.create(&mut session).await.unwrap();

    session
        .messages
        .push(Message::assistant(Some("回复".to_string())));
    session.message_count = 2;
    store.update(&mut session).await.unwrap();

    let loaded = store.get(&session.id).await.unwrap().unwrap();
    assert_eq!(loaded.messages.len(), 2);
    assert_eq!(loaded.messages[1].role, "assistant");
    // seq 连续递增
    assert_eq!(loaded.messages[0].seq, 1);
    assert_eq!(loaded.messages[1].seq, 2);
}

#[tokio::test]
async fn store_delete_removes_session() {
    let store = temp_store().await;
    let mut session = Session::new(None, None);
    store.create(&mut session).await.unwrap();

    let deleted = store.delete(&session.id).await.unwrap();
    assert!(deleted);

    assert!(store.get(&session.id).await.unwrap().is_none());
}

#[tokio::test]
async fn store_list_all_returns_sessions() {
    let store = temp_store().await;
    let mut s1 = Session::new(Some("会话1".to_string()), None);
    let mut s2 = Session::new(Some("会话2".to_string()), None);
    store.create(&mut s1).await.unwrap();
    store.create(&mut s2).await.unwrap();

    let list = store.list_all(10, 0).await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].title, Some("会话2".to_string()));
}

#[tokio::test]
async fn store_count_returns_correct_count() {
    let store = temp_store().await;
    assert_eq!(store.count().await.unwrap(), 0);
    store.create(&mut Session::new(None, None)).await.unwrap();
    assert_eq!(store.count().await.unwrap(), 1);
}

#[tokio::test]
async fn store_incremental_save_only_inserts_new() {
    let store = temp_store().await;
    let mut session = Session::new(None, None);
    session.messages.push(Message::user("m1".to_string()));
    session.messages.push(Message::user("m2".to_string()));
    store.create(&mut session).await.unwrap();

    session.messages.push(Message::user("m3".to_string()));
    store.update(&mut session).await.unwrap();

    let loaded = store.get(&session.id).await.unwrap().unwrap();
    assert_eq!(loaded.messages.len(), 3);
    assert_eq!(loaded.messages[2].content, Some("m3".to_string()));
    assert_eq!(loaded.messages[2].seq, 3);
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
    store.create(&mut session).await.unwrap();

    let loaded = store.get(&session.id).await.unwrap().unwrap();
    assert!(loaded.messages[0].tool_calls.is_some());
    assert_eq!(
        loaded.messages[0].tool_calls.as_ref().unwrap()[0]["id"],
        "call_1"
    );
}

// ===== mark_compaction 测试组 =====

#[tokio::test]
async fn mark_compaction_inserts_boundary_message() {
    let store = temp_store().await;
    let mut session = Session::new(None, None);
    session.messages.push(Message::user("hello".to_string()));
    session
        .messages
        .push(Message::assistant(Some("hi".to_string())));
    store.create(&mut session).await.unwrap();

    let new_seq = store
        .mark_compaction(
            &session.id,
            "## 目标\n- 测试".to_string(),
            CompressionReason::Auto,
        )
        .await
        .unwrap();

    // 新 seq = 3（前面有 2 条消息）
    assert_eq!(new_seq, 3);

    // 全量历史能看到这条边界
    let full = store.load_full_history(&session.id).await.unwrap();
    assert_eq!(full.len(), 3);
    let boundary = &full[2];
    assert_eq!(boundary.seq, 3);
    assert_eq!(boundary.kind, MessageKind::Compaction);
    assert_eq!(boundary.role, "system");
    assert_eq!(boundary.content.as_deref(), Some("## 目标\n- 测试"));
}

#[tokio::test]
async fn mark_compaction_updates_session_metadata() {
    let store = temp_store().await;
    let mut session = Session::new(None, None);
    store.create(&mut session).await.unwrap();

    let new_seq = store
        .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
        .await
        .unwrap();

    // 通过 get 验证 sessions 表更新
    let loaded = store.get(&session.id).await.unwrap().unwrap();
    assert_eq!(loaded.last_compacted_seq, Some(new_seq));
    assert_eq!(loaded.compression_count, 1);
    // 统计字段不重置（保持原值）
    assert_eq!(loaded.total_prompt_tokens, 0);
    // end_reason / ended_at 不被压缩改动
    assert!(loaded.ended_at.is_none());
    assert!(loaded.end_reason.is_none());
}

#[tokio::test]
async fn mark_compaction_returns_not_found_for_missing_session() {
    let store = temp_store().await;
    let result = store
        .mark_compaction("nonexistent", "摘要".to_string(), CompressionReason::Auto)
        .await;
    assert!(matches!(result, Err(SessionError::NotFound(_))));
}

// ===== update_system_prompt 测试组 =====

#[tokio::test]
async fn update_system_prompt_updates_db_row() {
    let store = temp_store().await;
    let mut session = Session::new(None, Some("旧提示词".to_string()));
    store.create(&mut session).await.unwrap();

    store
        .update_system_prompt(&session.id, "新提示词（压缩后重建）")
        .await
        .unwrap();

    // 通过 get 验证字段已更新，且其他字段未被破坏
    let loaded = store.get(&session.id).await.unwrap().unwrap();
    assert_eq!(
        loaded.system_prompt.as_deref(),
        Some("新提示词（压缩后重建）")
    );
    // 其他字段保持原值（create 时初始化的默认值）
    assert_eq!(loaded.compression_count, 0);
    assert!(loaded.last_compacted_seq.is_none());
}

#[tokio::test]
async fn update_system_prompt_errors_on_missing_session() {
    let store = temp_store().await;
    let result = store.update_system_prompt("nonexistent", "新提示词").await;
    assert!(matches!(result, Err(SessionError::NotFound(_))));
}

// ===== update_title 测试组 =====

#[tokio::test]
async fn update_title_updates_db_row() {
    let store = temp_store().await;
    let mut session = Session::new(None, None);
    // create 时 title 默认 "新会话"
    store.create(&mut session).await.unwrap();
    assert_eq!(session.title.as_deref(), Some("新会话"));

    store
        .update_title(&session.id, "Rust 异步讨论")
        .await
        .unwrap();

    // 通过 get 验证 title 已更新，且其他字段未被破坏
    let loaded = store.get(&session.id).await.unwrap().unwrap();
    assert_eq!(loaded.title.as_deref(), Some("Rust 异步讨论"));
    // 其他字段保持原值
    assert_eq!(loaded.compression_count, 0);
    assert!(loaded.last_compacted_seq.is_none());
}

#[tokio::test]
async fn update_title_errors_on_missing_session() {
    let store = temp_store().await;
    let result = store.update_title("nonexistent", "标题").await;
    assert!(matches!(result, Err(SessionError::NotFound(_))));
}

// ===== load_visible_messages 测试组 =====

#[tokio::test]
async fn load_visible_returns_all_when_never_compacted() {
    let store = temp_store().await;
    let mut session = Session::new(None, None);
    session.messages.push(Message::user("m1".to_string()));
    session.messages.push(Message::user("m2".to_string()));
    store.create(&mut session).await.unwrap();

    let visible = store.load_visible_messages(&session.id).await.unwrap();
    assert_eq!(visible.len(), 2);
}

#[tokio::test]
async fn load_visible_returns_only_after_boundary() {
    let store = temp_store().await;
    let mut session = Session::new(None, None);
    session.messages.push(Message::user("old1".to_string()));
    session.messages.push(Message::user("old2".to_string()));
    session.messages.push(Message::user("old3".to_string()));
    store.create(&mut session).await.unwrap();

    // 标记压缩：此时 seq=4 是 compaction 边界
    store
        .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
        .await
        .unwrap();

    // 再加一条新消息
    let mut session2 = store.get(&session.id).await.unwrap().unwrap();
    session2.messages.push(Message::user("new1".to_string()));
    store.update(&mut session2).await.unwrap();

    let visible = store.load_visible_messages(&session.id).await.unwrap();
    // 只看到 compaction 边界（seq=4）+ 之后的消息（seq=5）
    assert_eq!(visible.len(), 2);
    assert_eq!(visible[0].kind, MessageKind::Compaction);
    assert_eq!(visible[0].seq, 4);
    assert_eq!(visible[1].content.as_deref(), Some("new1"));
    assert_eq!(visible[1].seq, 5);
}

#[tokio::test]
async fn load_visible_returns_latest_after_multiple_compactions() {
    let store = temp_store().await;
    let mut session = Session::new(None, None);
    session.messages.push(Message::user("v1-1".to_string()));
    store.create(&mut session).await.unwrap();

    // 第一次压缩
    let seq1 = store
        .mark_compaction(&session.id, "摘要1".to_string(), CompressionReason::Auto)
        .await
        .unwrap();

    let mut s = store.get(&session.id).await.unwrap().unwrap();
    s.messages.push(Message::user("v2-1".to_string()));
    store.update(&mut s).await.unwrap();

    // 第二次压缩
    let seq2 = store
        .mark_compaction(&session.id, "摘要2".to_string(), CompressionReason::Auto)
        .await
        .unwrap();

    assert!(seq2 > seq1);

    let visible = store.load_visible_messages(&session.id).await.unwrap();
    // 只看到最近一次 compaction 边界
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].kind, MessageKind::Compaction);
    assert_eq!(visible[0].content.as_deref(), Some("摘要2"));
}

// ===== load_full_history 测试组 =====

#[tokio::test]
async fn load_full_history_includes_compacted_messages() {
    let store = temp_store().await;
    let mut session = Session::new(None, None);
    session.messages.push(Message::user("old".to_string()));
    session
        .messages
        .push(Message::assistant(Some("reply".to_string())));
    store.create(&mut session).await.unwrap();

    store
        .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
        .await
        .unwrap();

    let full = store.load_full_history(&session.id).await.unwrap();
    // 全量 = 2 条原始消息 + 1 条 compaction 边界
    assert_eq!(full.len(), 3);
    assert_eq!(full[2].kind, MessageKind::Compaction);
}
