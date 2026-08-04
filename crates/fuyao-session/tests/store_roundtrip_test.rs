//! fuyao-session 集成测试：SessionStore 持久化往返与多 session 隔离
//!
//! src/store/tests.rs 的 25 个单元测试已逐方法覆盖 CRUD / 消息插入 / 压缩边界 / 单字段更新。
//! 本文件聚焦**跨方法的持久化往返契约**——单测的空白带：
//!
//! - 消息插入 → count_messages / load_visible_messages 的统计与可见窗口一致性
//! - session.update 的全量字段持久化往返（update 后 get 读回，字段无漂移）
//! - **费用累积 + 持久化往返**（跨 cost ↔ store）：accumulate_session_total 改内存 Session →
//!   store.update 落库 → store.get 读回，验证 total_cost / token 累积跨 DB 往返精确
//! - **多 session 隔离**：N 个 session 各自插消息，互不串扰
//! - **完整 session 生命周期**（create → insert → accumulate cost → update → end_session → get）
//!
//! 全部使用默认配置（不调 set_config），走 get_config 未 set 返回 default 的兜底。

mod common;

use common::temp_store;
use fuyao_api::{ImageContent, Message, MessageKind, MessageRole, Session};
use fuyao_session::accumulate_session_total;

// ============================================================================
// 消息可见窗口与统计一致性
// ============================================================================

#[tokio::test]
async fn visible_messages_match_inserted_count() {
    // 插入 N 条消息后，count_messages 与 load_visible_messages 长度应一致（未经压缩）
    let store = temp_store().await;
    let session = Session::new(None, None, None);
    store.create(&session).await.unwrap();

    for i in 0..5 {
        let mut msg = Message::user(format!("消息_{i}"));
        store.insert_message(&session.id, &mut msg).await.unwrap();
    }

    let counted = store.count_messages(&session.id).await.unwrap();
    let visible = store
        .load_visible_messages(&session.id, usize::MAX)
        .await
        .unwrap();
    assert_eq!(counted, 5, "count_messages 应等于插入数");
    assert_eq!(visible.len(), 5, "可见消息数应等于插入数");
}

#[tokio::test]
async fn messages_preserve_role_and_content_on_roundtrip() {
    // 消息插入后读回，role / content / kind 应保持（持久化往返契约）
    let store = temp_store().await;
    let session = Session::new(None, None, None);
    store.create(&session).await.unwrap();

    let mut user_msg = Message::user("用户提问".to_string());
    store
        .insert_message(&session.id, &mut user_msg)
        .await
        .unwrap();
    let mut assistant_msg = Message::assistant(Some("助手回答".to_string()));
    assistant_msg.role = MessageRole::Assistant;
    store
        .insert_message(&session.id, &mut assistant_msg)
        .await
        .unwrap();

    let visible = store
        .load_visible_messages(&session.id, usize::MAX)
        .await
        .unwrap();
    assert_eq!(visible.len(), 2);
    assert_eq!(visible[0].role, MessageRole::User);
    assert_eq!(visible[0].content.as_deref(), Some("用户提问"));
    assert_eq!(visible[0].kind, MessageKind::Message);
    assert_eq!(visible[1].role, MessageRole::Assistant);
    assert_eq!(visible[1].content.as_deref(), Some("助手回答"));
}

#[tokio::test]
async fn images_preserve_on_roundtrip() {
    // 带图消息插入后读回，images 应完整保持（多图 + mime + base64 逐字段一致）
    let store = temp_store().await;
    let session = Session::new(None, None, None);
    store.create(&session).await.unwrap();

    let images = vec![
        ImageContent {
            mime_type: "image/png".into(),
            data: "aGVsbG8=".into(),
        },
        ImageContent {
            mime_type: "image/jpeg".into(),
            data: "d29ybGQ=".into(),
        },
    ];
    let mut msg = Message::user_with_images("看图说话".to_string(), images.clone());
    store.insert_message(&session.id, &mut msg).await.unwrap();

    let visible = store
        .load_visible_messages(&session.id, usize::MAX)
        .await
        .unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].images.len(), 2);
    assert_eq!(visible[0].images, images);

    // 全量历史读回同样带图
    let full = store.load_full_history(&session.id).await.unwrap();
    assert_eq!(full[0].images, images);
}

#[tokio::test]
async fn no_images_roundtrips_empty() {
    // 纯文本消息读回 images 恒为空（NULL 列 → 空 vec）
    let store = temp_store().await;
    let session = Session::new(None, None, None);
    store.create(&session).await.unwrap();

    let mut msg = Message::user("纯文本".to_string());
    store.insert_message(&session.id, &mut msg).await.unwrap();

    let visible = store
        .load_visible_messages(&session.id, usize::MAX)
        .await
        .unwrap();
    assert!(visible[0].images.is_empty());
}

#[tokio::test]
async fn seq_is_monotonically_increasing_across_inserts() {
    // insert_message 分配的 seq 应单调递增（UNIQUE(session_id, seq) 保证）
    let store = temp_store().await;
    let session = Session::new(None, None, None);
    store.create(&session).await.unwrap();

    let mut seqs = Vec::new();
    for i in 0..4 {
        let mut msg = Message::user(format!("m{i}"));
        store.insert_message(&session.id, &mut msg).await.unwrap();
        seqs.push(msg.seq);
    }

    // 每条 seq 严格递增
    for w in seqs.windows(2) {
        assert!(w[1] > w[0], "seq 应严格递增，得到 {:?}", seqs);
    }
}

// ============================================================================
// session.update 全量字段持久化往返
// ============================================================================

#[tokio::test]
async fn update_roundtrips_all_metadata_fields() {
    // update 修改的元数据字段（message_count / tool_call_count / 各 token 总计 / cost），
    // get 读回应完全一致——验证全量 UPDATE 的往返无丢失。
    let store = temp_store().await;
    let mut session = Session::new(None, None, None);
    store.create(&session).await.unwrap();

    // 业务层累积后修改元数据
    session.message_count = 42;
    session.tool_call_count = 7;
    session.total_prompt_tokens = 12345;
    session.total_completion_tokens = 678;
    session.total_reasoning_tokens = 100;
    session.total_cached_tokens = 200;
    session.total_cost = 0.123456;
    store.update(&session).await.unwrap();

    let read_back = store
        .get(&session.id)
        .await
        .unwrap()
        .expect("session 应存在");

    assert_eq!(read_back.message_count, 42);
    assert_eq!(read_back.tool_call_count, 7);
    assert_eq!(read_back.total_prompt_tokens, 12345);
    assert_eq!(read_back.total_completion_tokens, 678);
    assert_eq!(read_back.total_reasoning_tokens, 100);
    assert_eq!(read_back.total_cached_tokens, 200);
    assert!((read_back.total_cost - 0.123456).abs() < 1e-9);
}

#[tokio::test]
async fn update_title_and_system_prompt_roundtrip() {
    // 单字段更新（update_title / update_system_prompt）后 get 读回应反映新值
    let store = temp_store().await;
    let session = Session::new(None, None, None);
    store.create(&session).await.unwrap();

    store.update_title(&session.id, "新标题").await.unwrap();
    store
        .update_system_prompt(&session.id, "你是助手")
        .await
        .unwrap();

    let read_back = store.get(&session.id).await.unwrap().unwrap();
    assert_eq!(read_back.title.as_deref(), Some("新标题"));
    assert_eq!(read_back.system_prompt.as_deref(), Some("你是助手"));
}

// ============================================================================
// 费用累积 + 持久化往返（跨 cost ↔ store）
// ============================================================================

#[tokio::test]
async fn accumulated_cost_persists_through_update_roundtrip() {
    // 跨模块协作：accumulate_session_total（cost 模块）改内存 Session →
    // store.update（store 模块）落库 → store.get 读回。
    // 验证 Decimal 累积的 total_cost 跨 DB 往返无精度漂移。
    let store = temp_store().await;
    let mut session = Session::new(None, None, None);
    store.create(&session).await.unwrap();

    // 累积多条 assistant 消息（只有 assistant 角色accumulated）
    for _ in 0..5 {
        let mut msg = Message::assistant(Some("回答".to_string()));
        msg.cost = 0.001;
        accumulate_session_total(&mut session, &msg);
    }

    // 累积后落库
    store.update(&session).await.unwrap();
    let read_back = store.get(&session.id).await.unwrap().unwrap();

    // 5 × 0.001 = 0.005（Decimal 累积精确，往返无漂移）
    assert!(
        (read_back.total_cost - 0.005).abs() < 1e-9,
        "累积费用往返应精确，实际 = {}",
        read_back.total_cost
    );
}

#[tokio::test]
async fn accumulate_only_counts_assistant_messages_in_session() {
    // accumulate_session_total 只累积 assistant 角色；user/tool 消息不计。
    // 验证累积逻辑在「混合消息 + 落库」场景下只认 assistant。
    let store = temp_store().await;
    let mut session = Session::new(None, None, None);
    store.create(&session).await.unwrap();

    // 一条 user（不计）+ 一条 assistant（计）+ 一条 tool（不计）
    let user_msg = Message::user("hi".to_string());
    accumulate_session_total(&mut session, &user_msg);

    let mut assistant_msg = Message::assistant(Some("resp".to_string()));
    assistant_msg.prompt_tokens = 100;
    assistant_msg.completion_tokens = 50;
    assistant_msg.cost = 0.01;
    accumulate_session_total(&mut session, &assistant_msg);

    let tool_msg = Message::tool_result("c1".to_string(), "result".to_string());
    accumulate_session_total(&mut session, &tool_msg);

    store.update(&session).await.unwrap();
    let read_back = store.get(&session.id).await.unwrap().unwrap();

    // 只有 assistant 计入：token 和 cost 都只反映 assistant 那条
    assert_eq!(read_back.total_prompt_tokens, 100);
    assert_eq!(read_back.total_completion_tokens, 50);
    assert!((read_back.total_cost - 0.01).abs() < 1e-9);
}

// ============================================================================
// 多 session 隔离
// ============================================================================

#[tokio::test]
async fn multiple_sessions_isolate_messages() {
    // 两个 session 各自插入消息，load_visible_messages 不应串扰
    let store = temp_store().await;
    let session_a = Session::new(None, None, None);
    let session_b = Session::new(None, None, None);
    store.create(&session_a).await.unwrap();
    store.create(&session_b).await.unwrap();

    for i in 0..3 {
        let mut msg = Message::user(format!("A_{i}"));
        store.insert_message(&session_a.id, &mut msg).await.unwrap();
    }
    for i in 0..2 {
        let mut msg = Message::user(format!("B_{i}"));
        store.insert_message(&session_b.id, &mut msg).await.unwrap();
    }

    let visible_a = store
        .load_visible_messages(&session_a.id, usize::MAX)
        .await
        .unwrap();
    let visible_b = store
        .load_visible_messages(&session_b.id, usize::MAX)
        .await
        .unwrap();

    assert_eq!(visible_a.len(), 3);
    assert_eq!(visible_b.len(), 2);
    // A 的消息内容都以 A_ 开头，不含 B 的
    assert!(
        visible_a
            .iter()
            .all(|m| m.content.as_deref().unwrap_or("").starts_with("A_"))
    );
    assert!(
        visible_b
            .iter()
            .all(|m| m.content.as_deref().unwrap_or("").starts_with("B_"))
    );
}

#[tokio::test]
async fn count_and_list_all_reflect_multiple_sessions() {
    // count / list_all 应正确反映多 session 状态
    let store = temp_store().await;
    for _ in 0..3 {
        let session = Session::new(None, None, None);
        store.create(&session).await.unwrap();
    }

    assert_eq!(store.count().await.unwrap(), 3);
    let listed = store.list_all(None, 100, 0).await.unwrap();
    assert_eq!(listed.len(), 3);
}

// ============================================================================
// 完整 session 生命周期
// ============================================================================

#[tokio::test]
async fn full_session_lifecycle_create_to_end() {
    // 端到端 happy-path：create → insert → accumulate cost → update → end_session → get 读回 ended_at
    // 这条跨多方法的链路单测从未覆盖
    let store = temp_store().await;
    let mut session = Session::new(None, None, None);
    store.create(&session).await.unwrap();

    // 插入对话
    let mut user_msg = Message::user("开始任务".to_string());
    store
        .insert_message(&session.id, &mut user_msg)
        .await
        .unwrap();
    let mut assistant_msg = Message::assistant(Some("任务完成".to_string()));
    assistant_msg.prompt_tokens = 200;
    assistant_msg.completion_tokens = 80;
    assistant_msg.cost = 0.02;
    store
        .insert_message(&session.id, &mut assistant_msg)
        .await
        .unwrap();

    // 累积费用并落库
    accumulate_session_total(&mut session, &assistant_msg);
    store.update(&session).await.unwrap();

    // 结束会话
    store.end_session(&session.id, "正常结束").await.unwrap();

    let read_back = store.get(&session.id).await.unwrap().unwrap();
    assert!(read_back.ended_at.is_some(), "结束后 ended_at 应被设置");
    assert_eq!(read_back.end_reason.as_deref(), Some("正常结束"));
    assert!((read_back.total_cost - 0.02).abs() < 1e-9);
    assert_eq!(read_back.total_prompt_tokens, 200);
}
