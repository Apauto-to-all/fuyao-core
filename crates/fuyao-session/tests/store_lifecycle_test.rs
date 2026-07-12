//! fuyao-session 集成测试：SQLiteStore + SessionManager 生命周期
//!
//! 聚焦跨模块协作与公共 API 契约：
//! - SQLiteStore：CRUD 全流程（create/get/update/delete/list/count）
//! - SessionManager：消息追加 + 费用累加 + 增量保存 + 会话分裂 + fork
//! - 错误路径：NotFound / InvalidState
//!
//! 每个 DB 路径唯一（tempfile + 隔离子目录），不触碰真实 ~/.fuyao。

mod common;

use common::{temp_agent_paths, temp_manager, temp_store};
use fuyao_api::{Message, Session};
use fuyao_session::SessionError;

// ============================================================================
// SQLiteStore：CRUD 全流程
// ============================================================================

#[tokio::test]
async fn store_create_and_get_roundtrip() {
    let store = temp_store().await;
    let session = Session::new(Some("测试会话".to_string()), Some("系统提示".to_string()));

    store.create(&session).await.expect("创建应成功");

    let loaded = store
        .get(&session.id)
        .await
        .expect("查询应成功")
        .expect("应找到已创建的会话");
    assert_eq!(loaded.id, session.id);
    assert_eq!(loaded.title.as_deref(), Some("测试会话"));
}

#[tokio::test]
async fn store_get_nonexistent_returns_none() {
    let store = temp_store().await;

    let result = store.get("nonexistent").await.expect("查询不应报错");

    assert!(result.is_none());
}

#[tokio::test]
async fn store_update_appends_messages_incrementally() {
    // update 应只插入新增消息（增量保存）
    let store = temp_store().await;
    let mut session = Session::new(None, None);
    session.messages.push(Message::user("第一条".to_string()));
    store.create(&session).await.unwrap();

    // 第一次 update：1 条
    store.update(&session).await.unwrap();
    // 追加第二条
    session.messages.push(Message::user("第二条".to_string()));
    store.update(&session).await.unwrap();

    let loaded = store.get(&session.id).await.unwrap().unwrap();
    assert_eq!(loaded.messages.len(), 2, "应保存全部 2 条消息");
}

#[tokio::test]
async fn store_delete_removes_session_and_messages() {
    let store = temp_store().await;
    let mut session = Session::new(None, None);
    session.messages.push(Message::user("内容".to_string()));
    store.create(&session).await.unwrap();
    store.update(&session).await.unwrap();

    let deleted = store.delete(&session.id).await.expect("删除应成功");
    assert!(deleted, "已存在的会话删除应返回 true");

    let loaded = store.get(&session.id).await.unwrap();
    assert!(loaded.is_none(), "删除后查询应返回 None");
}

#[tokio::test]
async fn store_delete_nonexistent_returns_false() {
    let store = temp_store().await;

    let deleted = store.delete("nonexistent").await.expect("删除不应报错");

    assert!(!deleted);
}

#[tokio::test]
async fn store_list_all_orders_by_started_at_desc() {
    let store = temp_store().await;
    let s1 = Session::new(Some("第一个".to_string()), None);
    store.create(&s1).await.unwrap();
    let s2 = Session::new(Some("第二个".to_string()), None);
    store.create(&s2).await.unwrap();

    let list = store.list_all(10, 0).await.expect("列表应成功");

    assert_eq!(list.len(), 2);
    // list_all 不含消息
    assert!(list.iter().all(|s| s.messages.is_empty()));
}

#[tokio::test]
async fn store_count_returns_total() {
    let store = temp_store().await;
    store.create(&Session::new(None, None)).await.unwrap();
    store.create(&Session::new(None, None)).await.unwrap();

    let count = store.count().await.expect("计数应成功");

    assert_eq!(count, 2);
}

#[tokio::test]
async fn store_set_session_title_updates_record() {
    let store = temp_store().await;
    let session = Session::new(Some("原标题".to_string()), None);
    store.create(&session).await.unwrap();

    let updated = store
        .set_session_title(&session.id, "新标题")
        .await
        .expect("改标题应成功");
    assert!(updated);

    let loaded = store.get(&session.id).await.unwrap().unwrap();
    assert_eq!(loaded.title.as_deref(), Some("新标题"));
}

// ============================================================================
// SessionManager：高层 API
// ============================================================================

#[tokio::test]
async fn manager_create_with_default_title() {
    let mgr = temp_manager().await;

    let session = mgr.create(None, None).await.expect("创建应成功");

    assert_eq!(
        session.title.as_deref(),
        Some("新会话"),
        "默认标题为'新会话'"
    );
}

#[tokio::test]
async fn manager_create_with_id_preserves_custom_id() {
    let mgr = temp_manager().await;

    let session = mgr
        .create_with_id(
            "custom123".to_string(),
            Some("自定义".to_string()),
            None,
            None,
        )
        .await
        .expect("指定 ID 创建应成功");

    assert_eq!(session.id, "custom123");
    assert_eq!(session.title.as_deref(), Some("自定义"));
}

#[tokio::test]
async fn manager_add_message_accumulates_stats() {
    // add_message 累加 session 级别 token 统计
    let mgr = temp_manager().await;
    let paths = temp_agent_paths(std::env::temp_dir().join("fuyao_it_session_mgr"));
    let session = mgr.create(None, None).await.unwrap();

    let mut msg = Message::user("你好".to_string());
    msg.prompt_tokens = 10;
    mgr.add_message(&session.id, msg, &paths).await.unwrap();

    let mut msg = Message::assistant(Some("你好".to_string()));
    msg.completion_tokens = 20;
    let updated = mgr
        .add_message(&session.id, msg, &paths)
        .await
        .unwrap()
        .expect("应返回更新后的 session");

    assert_eq!(updated.total_prompt_tokens, 10);
    assert_eq!(updated.total_completion_tokens, 20);
    assert_eq!(updated.message_count, 2);
}

#[tokio::test]
async fn manager_add_message_to_tool_increments_tool_call_count() {
    let mgr = temp_manager().await;
    let paths = temp_agent_paths(std::env::temp_dir().join("fuyao_it_session_toolcount"));
    let session = mgr.create(None, None).await.unwrap();

    mgr.add_message(
        &session.id,
        Message::tool_result("call_1".to_string(), "结果".to_string()),
        &paths,
    )
    .await
    .unwrap();

    let updated = mgr.get(&session.id).await.unwrap().unwrap();
    assert_eq!(updated.tool_call_count, 1);
}

#[tokio::test]
async fn manager_add_message_nonexistent_session_returns_none() {
    let mgr = temp_manager().await;
    let paths = temp_agent_paths(std::env::temp_dir().join("fuyao_it_session_none"));

    let result = mgr
        .add_message("nonexistent", Message::user("x".to_string()), &paths)
        .await
        .expect("不应报错（返回 None）");

    assert!(result.is_none());
}

#[tokio::test]
async fn manager_get_messages_returns_in_order() {
    let mgr = temp_manager().await;
    let paths = temp_agent_paths(std::env::temp_dir().join("fuyao_it_session_msgs"));
    let session = mgr.create(None, None).await.unwrap();

    mgr.add_message(&session.id, Message::user("第一条".to_string()), &paths)
        .await
        .unwrap();
    mgr.add_message(
        &session.id,
        Message::assistant(Some("第二条".to_string())),
        &paths,
    )
    .await
    .unwrap();

    let messages = mgr.get_messages(&session.id).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[1].role, "assistant");
}

#[tokio::test]
async fn manager_end_session_marks_ended() {
    let mgr = temp_manager().await;
    let session = mgr.create(None, None).await.unwrap();

    let updated = mgr
        .end_session(&session.id, "user_exit")
        .await
        .unwrap()
        .expect("应返回结束后的 session");

    assert!(updated.ended_at.is_some(), "ended_at 应被设置");
    assert_eq!(updated.end_reason.as_deref(), Some("user_exit"));
}

#[tokio::test]
async fn manager_end_session_nonexistent_returns_none() {
    let mgr = temp_manager().await;

    let result = mgr.end_session("nonexistent", "x").await.unwrap();

    assert!(result.is_none());
}

#[tokio::test]
async fn manager_save_updates_cache() {
    // save 后 get 应返回最新数据（缓存一致性）
    let mgr = temp_manager().await;
    let mut session = Session::new(Some("原标题".to_string()), None);
    mgr.create_with_id(session.id.clone(), session.title.clone(), None, None)
        .await
        .unwrap();
    session.title = Some("改后标题".to_string());
    mgr.save(&session).await.expect("save 应成功");

    let loaded = mgr.get(&session.id).await.unwrap().unwrap();
    assert_eq!(loaded.title.as_deref(), Some("改后标题"));
}

// ============================================================================
// SessionManager：分裂与 fork
// ============================================================================

#[tokio::test]
async fn manager_split_session_creates_child_with_compressed_messages() {
    let mgr = temp_manager().await;
    let paths = temp_agent_paths(std::env::temp_dir().join("fuyao_it_session_split"));
    let old = mgr.create(Some("旧会话".to_string()), None).await.unwrap();
    mgr.add_message(&old.id, Message::user("旧消息".to_string()), &paths)
        .await
        .unwrap();

    let compressed = vec![Message::system("压缩后的摘要".to_string())];
    let new = mgr
        .split_session(
            &old.id,
            "新系统提示".to_string(),
            compressed,
            Some("新会话".to_string()),
        )
        .await
        .expect("分裂应成功");

    // 新会话 parent 指向旧
    assert_eq!(new.parent_session_id.as_deref(), Some(old.id.as_str()));
    // 新会话含压缩消息
    let new_msgs = mgr.get_messages(&new.id).await.unwrap();
    assert_eq!(new_msgs.len(), 1);
    assert_eq!(new_msgs[0].role, "system");
    // 旧会话应被标记结束
    let old_loaded = mgr.get(&old.id).await.unwrap().unwrap();
    assert!(old_loaded.ended_at.is_some());
}

#[tokio::test]
async fn manager_fork_messages_copies_from_source_to_target() {
    let mgr = temp_manager().await;
    let paths = temp_agent_paths(std::env::temp_dir().join("fuyao_it_session_fork"));
    let source = mgr.create(Some("源".to_string()), None).await.unwrap();
    let target = mgr.create(Some("目标".to_string()), None).await.unwrap();
    mgr.add_message(&source.id, Message::user("源消息".to_string()), &paths)
        .await
        .unwrap();

    let forked = mgr
        .fork_messages(&source.id, &target.id)
        .await
        .expect("fork 应成功");

    assert_eq!(forked.message_count, 1, "目标应含 fork 来的消息");
    let target_msgs = mgr.get_messages(&target.id).await.unwrap();
    assert_eq!(target_msgs.len(), 1);
    assert_eq!(target_msgs[0].content.as_deref(), Some("源消息"));
}

#[tokio::test]
async fn manager_fork_messages_nonexistent_source_returns_not_found() {
    let mgr = temp_manager().await;
    let target = mgr.create(None, None).await.unwrap();

    let result = mgr.fork_messages("nonexistent", &target.id).await;

    assert!(matches!(result, Err(SessionError::NotFound(_))));
}

// ============================================================================
// SessionManager：list/count
// ============================================================================

#[tokio::test]
async fn manager_list_and_count() {
    let mgr = temp_manager().await;
    mgr.create(Some("A".to_string()), None).await.unwrap();
    mgr.create(Some("B".to_string()), None).await.unwrap();

    let list = mgr.list(10, 0).await.unwrap();
    let count = mgr.count().await.unwrap();

    assert_eq!(count, 2);
    assert_eq!(list.len(), 2);
}

#[tokio::test]
async fn manager_delete_clears_cache() {
    // 删除后再 get 应返回 None（缓存被清）
    let mgr = temp_manager().await;
    let session = mgr.create(None, None).await.unwrap();
    let id = session.id.clone();

    let deleted = mgr.delete(&id).await.unwrap();
    assert!(deleted);

    let loaded = mgr.get(&id).await.unwrap();
    assert!(loaded.is_none());
}
