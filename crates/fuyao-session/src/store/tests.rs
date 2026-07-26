//! SessionStore 单元测试（CRUD + 上下文压缩边界 + 事件级落库）

use super::SessionStore;
use super::compaction::CompressionReason;
use crate::error::SessionError;
use fuyao_api::{Message, MessageKind, MessageRole, Session};
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

// ===== 基础 CRUD 测试（元数据 only——消息已脱离 session 内存） =====

#[tokio::test]
async fn store_create_and_get() {
    let store = temp_store().await;
    let session = Session::new(Some("测试".to_string()), None);
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
async fn store_update_syncs_metadata() {
    let store = temp_store().await;
    let mut session = Session::new(None, None);
    store.create(&session).await.unwrap();

    // 改一些元数据后 update
    session.total_prompt_tokens = 12345;
    session.compression_count = 2;
    store.update(&session).await.unwrap();

    let loaded = store.get(&session.id).await.unwrap().unwrap();
    assert_eq!(loaded.total_prompt_tokens, 12345);
    assert_eq!(loaded.compression_count, 2);
}

// ===== parent_session_id（通用子任务标记）测试组 =====

#[tokio::test]
async fn parent_session_id_defaults_none_on_create() {
    // 主 session（create_session / resume_session 产出）：parent_session_id 应为 None
    let store = temp_store().await;
    let session = Session::new(None, None);
    store.create(&session).await.unwrap();

    let loaded = store.get(&session.id).await.unwrap().unwrap();
    assert!(loaded.parent_session_id.is_none());
}

#[tokio::test]
async fn parent_session_id_persists_and_roundtrips() {
    // 子任务 session：parent_session_id 应为父 id，且 create/get/update 全程保持
    let store = temp_store().await;
    let parent = Session::new(Some("父会话".to_string()), None);
    store.create(&parent).await.unwrap();

    let mut child = Session::new(None, None);
    child.parent_session_id = Some(parent.id.clone());
    store.create(&child).await.unwrap();

    // get 读回
    let loaded = store.get(&child.id).await.unwrap().unwrap();
    assert_eq!(
        loaded.parent_session_id.as_deref(),
        Some(parent.id.as_str())
    );

    // update 不丢字段（全量 UPDATE 应保持 parent_session_id）
    let mut modified = loaded;
    modified.message_count = 5;
    store.update(&modified).await.unwrap();
    let reloaded = store.get(&child.id).await.unwrap().unwrap();
    assert_eq!(
        reloaded.parent_session_id.as_deref(),
        Some(parent.id.as_str())
    );
    assert_eq!(reloaded.message_count, 5);
}

#[tokio::test]
async fn fork_copy_visible_messages_to_child() {
    // 模拟 Engine 的 fork 拷贝数据层机制（build_forked_session，fork_session 与
    // create_child_session 的 Fork 模式共用）：
    // 源 session 有可见消息 → 新建子 session → 逐条 insert_message 复制 →
    // 子 session 的可见窗口应与源一致（含 compaction 边界，若有）
    let store = temp_store().await;

    // 源 session：2 条普通消息
    let parent = Session::new(None, Some("父系统提示词".to_string()));
    store.create(&parent).await.unwrap();
    let mut m1 = Message::user("父消息1".to_string());
    store.insert_message(&parent.id, &mut m1).await.unwrap();
    let mut m2 = Message::assistant(Some("父回复".to_string()));
    store.insert_message(&parent.id, &mut m2).await.unwrap();

    // 加一次压缩，让源 session 的可见窗口含 compaction 边界
    store
        .mark_compaction(&parent.id, "父摘要".to_string(), CompressionReason::Auto)
        .await
        .unwrap();
    let mut m3 = Message::user("压缩后消息".to_string());
    store.insert_message(&parent.id, &mut m3).await.unwrap();

    let source_visible = store.load_visible_messages(&parent.id).await.unwrap();
    // 源可见 = [compaction 边界, 压缩后消息]
    assert_eq!(source_visible.len(), 2);

    // 构造子 session（复制源系统提示词 + 标记 parent_session_id）
    // message_count 只计普通消息（排除 compaction 边界，与 count_messages 语义一致）
    let non_compaction_count = source_visible
        .iter()
        .filter(|m| matches!(m.kind, MessageKind::Message))
        .count();
    let mut child = Session::new(None, parent.system_prompt.clone());
    child.parent_session_id = Some(parent.id.clone());
    child.message_count = non_compaction_count as i64;
    store.create(&child).await.unwrap();

    // 逐条复制可见消息（与 fork_session / apply 的 copy-to-new-seq 模式一致）
    for msg in &source_visible {
        let mut clone = msg.clone();
        clone.seq = 0;
        store.insert_message(&child.id, &mut clone).await.unwrap();
    }

    // 子 session 的可见窗口应与源一致（compaction 边界 + 压缩后消息）
    let child_visible = store.load_visible_messages(&child.id).await.unwrap();
    assert_eq!(child_visible.len(), source_visible.len());
    assert_eq!(child_visible[0].kind, MessageKind::Compaction);
    assert_eq!(child_visible[0].content.as_deref(), Some("父摘要"));
    assert_eq!(child_visible[1].content.as_deref(), Some("压缩后消息"));

    // 子 session 自身统计从 0 起算（费用 / token 不继承源），仅 message_count 对齐复制条数
    let child_meta = store.get(&child.id).await.unwrap().unwrap();
    assert_eq!(
        child_meta.parent_session_id.as_deref(),
        Some(parent.id.as_str())
    );
    // message_count 只计普通消息（1 条），不含 compaction 边界
    assert_eq!(child_meta.message_count, 1);
    assert_eq!(child_meta.total_prompt_tokens, 0);
    assert_eq!(child_meta.total_cost, 0.0);
    // 子 session 从未压缩过：自己的压缩指针为空（源的压缩边界只是被复制成普通行）
    assert!(child_meta.last_compacted_seq.is_none());

    // count_messages 排除 compaction 边界，与 message_count 语义一致
    assert_eq!(
        store.count_messages(&child.id).await.unwrap(),
        non_compaction_count as i64
    );
}

// ===== insert_message / count_messages 测试（事件级落库） =====

#[tokio::test]
async fn insert_message_assigns_sequential_seq() {
    let store = temp_store().await;
    let session = Session::new(None, None);
    store.create(&session).await.unwrap();

    let mut m1 = Message::user("第一条".to_string());
    let seq1 = store.insert_message(&session.id, &mut m1).await.unwrap();
    assert_eq!(seq1, 1);
    assert_eq!(m1.seq, 1, "msg.seq 应被回填");

    let mut m2 = Message::assistant(Some("回复".to_string()));
    let seq2 = store.insert_message(&session.id, &mut m2).await.unwrap();
    assert_eq!(seq2, 2);
    assert_eq!(m2.seq, 2);
}

#[tokio::test]
async fn insert_message_serializes_tool_calls() {
    let store = temp_store().await;
    let session = Session::new(None, None);
    store.create(&session).await.unwrap();

    let mut msg = Message::assistant(None);
    msg.tool_calls = Some(serde_json::json!([{
        "id": "call_1",
        "type": "function",
        "function": { "name": "bash", "arguments": "{}" }
    }]));
    store.insert_message(&session.id, &mut msg).await.unwrap();

    let visible = store.load_visible_messages(&session.id).await.unwrap();
    assert_eq!(visible.len(), 1);
    assert!(visible[0].tool_calls.is_some());
    assert_eq!(visible[0].tool_calls.as_ref().unwrap()[0]["id"], "call_1");
}

#[tokio::test]
async fn count_messages_excludes_compaction_boundary() {
    let store = temp_store().await;
    let session = Session::new(None, None);
    store.create(&session).await.unwrap();

    let mut m1 = Message::user("a".to_string());
    store.insert_message(&session.id, &mut m1).await.unwrap();
    let mut m2 = Message::user("b".to_string());
    store.insert_message(&session.id, &mut m2).await.unwrap();

    assert_eq!(store.count_messages(&session.id).await.unwrap(), 2);

    // 压缩边界消息（kind='compaction'）不计入
    store
        .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
        .await
        .unwrap();
    assert_eq!(
        store.count_messages(&session.id).await.unwrap(),
        2,
        "count_messages 应排除 compaction 边界"
    );
}

// ===== mark_compaction 测试组 =====

#[tokio::test]
async fn mark_compaction_inserts_boundary_message() {
    let store = temp_store().await;
    let session = Session::new(None, None);
    store.create(&session).await.unwrap();

    let mut m1 = Message::user("hello".to_string());
    store.insert_message(&session.id, &mut m1).await.unwrap();
    let mut m2 = Message::assistant(Some("hi".to_string()));
    store.insert_message(&session.id, &mut m2).await.unwrap();

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
    assert_eq!(boundary.role, MessageRole::System);
    assert_eq!(boundary.content.as_deref(), Some("## 目标\n- 测试"));
}

#[tokio::test]
async fn mark_compaction_updates_session_metadata() {
    let store = temp_store().await;
    let session = Session::new(None, None);
    store.create(&session).await.unwrap();

    let new_seq = store
        .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
        .await
        .unwrap();

    let loaded = store.get(&session.id).await.unwrap().unwrap();
    assert_eq!(loaded.last_compacted_seq, Some(new_seq));
    assert_eq!(loaded.compression_count, 1);
    assert_eq!(loaded.total_prompt_tokens, 0);
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
    let session = Session::new(None, Some("旧提示词".to_string()));
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

// ===== update_title 测试组 =====

#[tokio::test]
async fn update_title_updates_db_row() {
    let store = temp_store().await;
    let session = Session::new(None, None);
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

// ===== end_session 测试组 =====

#[tokio::test]
async fn end_session_updates_db_row() {
    let store = temp_store().await;
    let session = Session::new(None, None);
    store.create(&session).await.unwrap();
    // 新建 session 未结束
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
    // 其他字段不动
    assert_eq!(loaded.compression_count, 0);
    assert!(loaded.last_compacted_seq.is_none());
}

#[tokio::test]
async fn end_session_errors_on_missing_session() {
    let store = temp_store().await;
    let result = store.end_session("nonexistent", "session_ended").await;
    assert!(matches!(result, Err(SessionError::NotFound(_))));
}

// ===== load_visible_messages 测试组 =====

#[tokio::test]
async fn load_visible_returns_all_when_never_compacted() {
    let store = temp_store().await;
    let session = Session::new(None, None);
    store.create(&session).await.unwrap();

    let mut m1 = Message::user("m1".to_string());
    store.insert_message(&session.id, &mut m1).await.unwrap();
    let mut m2 = Message::user("m2".to_string());
    store.insert_message(&session.id, &mut m2).await.unwrap();

    let visible = store.load_visible_messages(&session.id).await.unwrap();
    assert_eq!(visible.len(), 2);
}

#[tokio::test]
async fn load_visible_returns_only_after_boundary() {
    let store = temp_store().await;
    let session = Session::new(None, None);
    store.create(&session).await.unwrap();

    for content in ["old1", "old2", "old3"] {
        let mut msg = Message::user(content.to_string());
        store.insert_message(&session.id, &mut msg).await.unwrap();
    }

    // 标记压缩：此时 seq=4 是 compaction 边界
    store
        .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
        .await
        .unwrap();

    // 再加一条新消息
    let mut new_msg = Message::user("new1".to_string());
    store
        .insert_message(&session.id, &mut new_msg)
        .await
        .unwrap();

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
    let session = Session::new(None, None);
    store.create(&session).await.unwrap();

    let mut m = Message::user("v1-1".to_string());
    store.insert_message(&session.id, &mut m).await.unwrap();

    // 第一次压缩
    let seq1 = store
        .mark_compaction(&session.id, "摘要1".to_string(), CompressionReason::Auto)
        .await
        .unwrap();

    let mut m = Message::user("v2-1".to_string());
    store.insert_message(&session.id, &mut m).await.unwrap();

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
    let session = Session::new(None, None);
    store.create(&session).await.unwrap();

    let mut m1 = Message::user("old".to_string());
    store.insert_message(&session.id, &mut m1).await.unwrap();
    let mut m2 = Message::assistant(Some("reply".to_string()));
    store.insert_message(&session.id, &mut m2).await.unwrap();

    store
        .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
        .await
        .unwrap();

    let full = store.load_full_history(&session.id).await.unwrap();
    // 全量 = 2 条原始消息 + 1 条 compaction 边界
    assert_eq!(full.len(), 3);
    assert_eq!(full[2].kind, MessageKind::Compaction);
}
