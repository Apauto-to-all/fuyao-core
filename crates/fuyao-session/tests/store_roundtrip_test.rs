//! fuyao-session 集成测试：SessionStore 持久化往返与多 session 隔离
//!
//! 本文件聚焦**跨方法的持久化往返契约**——单测的空白带：
//!
//! - 消息插入 → load_visible_messages 的可见消息一致性
//! - session 元数据(message_count / tool_call_count / 各 token 总计 / cost)的持久化往返:
//!   这些字段由 insert_message 事务内原子累加(assistant 消息贡献 token/cost,
//!   tool 消息贡献 tool_call_count),经 store.get 读回验证无漂移
//! - **费用累积持久化往返**:insert_message 插入带 cost 的 assistant 消息 →
//!   store.get 读回,验证 total_cost / token 累积跨 DB 往返精确
//! - **多 session 隔离**:N 个 session 各自插消息,互不串扰
//! - **完整 session 生命周期**(create → insert_message 累加消费 → end_session → get 读回 ended_at)
//!
//! 全部使用默认配置(不调 set_config),走 get_config 未 set 返回 default 的兜底。

mod common;

use common::temp_store;
use fuyao_api::{ImageContent, Message, MessageKind, MessageRole, Session};

// ============================================================================
// 消息可见性与统计一致性
// ============================================================================

#[tokio::test]
async fn visible_messages_match_inserted_count() {
    // 插入 N 条消息后，load_visible_messages 长度应等于插入数（未经压缩）
    let store = temp_store().await;
    let session = Session::new(None, None, None);
    store.create(&session).await.unwrap();

    for i in 0..5 {
        let mut msg = Message::user(format!("消息_{i}"));
        store.insert_message(&session.id, &mut msg).await.unwrap();
    }

    let visible = store.load_visible_messages(&session.id).await.unwrap();
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

    let visible = store.load_visible_messages(&session.id).await.unwrap();
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

    let visible = store.load_visible_messages(&session.id).await.unwrap();
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

    let visible = store.load_visible_messages(&session.id).await.unwrap();
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
// session 元数据持久化往返(insert_message 事务内累加)
// ============================================================================

#[tokio::test]
async fn insert_message_accumulates_metadata_roundtrip() {
    // message_count / tool_call_count / 各 token 总计 / cost 现由 insert_message 事务内累加:
    // - 普通消息(kind=Message):message_count += 1
    // - role=Tool 的普通消息:额外 tool_call_count += 1
    // - assistant 消息:贡献 prompt/completion/reasoning/cached token 与 cost
    //   (user/tool 消息这些字段恒为 0,加 0 无害)
    // 本测试通过插入混合消息构造可精确断言的累计值,get 读回验证往返无丢失。
    let store = temp_store().await;
    let session = Session::new(None, None, None);
    store.create(&session).await.unwrap();

    // 3 条 user → message_count = 3
    for i in 0..3 {
        let mut msg = Message::user(format!("u{i}"));
        store.insert_message(&session.id, &mut msg).await.unwrap();
    }
    // 1 条 assistant 带 token/cost → message_count = 4,token 四项与 cost 落库
    let mut assistant = Message::assistant(Some("回答".to_string()));
    assistant.prompt_tokens = 12345;
    assistant.completion_tokens = 678;
    assistant.reasoning_tokens = 100;
    assistant.cached_tokens = 200;
    assistant.cost = 0.123456;
    store
        .insert_message(&session.id, &mut assistant)
        .await
        .unwrap();
    // 7 条 tool 结果 → message_count = 11,tool_call_count = 7
    for i in 0..7 {
        let mut msg = Message::tool_result(format!("c{i}"), "echo".to_string(), "结果".to_string());
        store.insert_message(&session.id, &mut msg).await.unwrap();
    }

    let read_back = store
        .get(&session.id)
        .await
        .unwrap()
        .expect("session 应存在");

    assert_eq!(read_back.message_count, 11);
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
// 费用累积持久化往返(insert_message 事务内累加 cost)
// ============================================================================

#[tokio::test]
async fn accumulated_cost_persists_through_insert_roundtrip() {
    // 费用累积现由 insert_message 事务内的 `total_cost += msg.cost` 完成
    // (DB 原子自增,DB 唯一数据源)。插入多条带 cost 的 assistant 消息后,
    // get 读回验证累积费用跨 DB 往返无精度漂移。
    let store = temp_store().await;
    let session = Session::new(None, None, None);
    store.create(&session).await.unwrap();

    // 累积 5 条带 cost 的 assistant 消息(insert_message 事务内累加 total_cost)
    for _ in 0..5 {
        let mut msg = Message::assistant(Some("回答".to_string()));
        msg.cost = 0.001;
        store.insert_message(&session.id, &mut msg).await.unwrap();
    }

    let read_back = store.get(&session.id).await.unwrap().unwrap();

    // 5 × 0.001 = 0.005(DB 累加,往返无漂移)
    assert!(
        (read_back.total_cost - 0.005).abs() < 1e-9,
        "累积费用往返应精确，实际 = {}",
        read_back.total_cost
    );
}

#[tokio::test]
async fn insert_message_only_counts_assistant_tokens_and_cost() {
    // insert_message 对 token 四项与 cost 始终累加,但 user/tool 消息这些字段恒为 0,
    // 语义上等价于「只有 assistant 消息贡献 token/cost」。
    // 本测试验证混合消息落库场景下,token/cost 只反映 assistant 那条。
    let store = temp_store().await;
    let session = Session::new(None, None, None);
    store.create(&session).await.unwrap();

    // 一条 user(token/cost 为 0)+ 一条 assistant(带 token/cost)+ 一条 tool(token/cost 为 0)
    let mut user_msg = Message::user("hi".to_string());
    store
        .insert_message(&session.id, &mut user_msg)
        .await
        .unwrap();

    let mut assistant_msg = Message::assistant(Some("resp".to_string()));
    assistant_msg.prompt_tokens = 100;
    assistant_msg.completion_tokens = 50;
    assistant_msg.cost = 0.01;
    store
        .insert_message(&session.id, &mut assistant_msg)
        .await
        .unwrap();

    let mut tool_msg =
        Message::tool_result("c1".to_string(), "echo".to_string(), "result".to_string());
    store
        .insert_message(&session.id, &mut tool_msg)
        .await
        .unwrap();

    let read_back = store.get(&session.id).await.unwrap().unwrap();

    // 只有 assistant 计入:token 和 cost 都只反映 assistant 那条
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

    let visible_a = store.load_visible_messages(&session_a.id).await.unwrap();
    let visible_b = store.load_visible_messages(&session_b.id).await.unwrap();

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

    assert_eq!(store.count_with_filter(None).await.unwrap(), 3);
    let listed = store.list_all(None, 100, 0).await.unwrap();
    assert_eq!(listed.len(), 3);
}

// ============================================================================
// 完整 session 生命周期
// ============================================================================

#[tokio::test]
async fn full_session_lifecycle_create_to_end() {
    // 端到端 happy-path:create → insert_message(事务内累加消费) → end_session → get 读回 ended_at。
    // 这条跨多方法的链路单测从未覆盖。
    let store = temp_store().await;
    let session = Session::new(None, None, None);
    store.create(&session).await.unwrap();

    // 插入对话:assistant 消息的 token/cost 由 insert_message 事务内累加进 sessions 表
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

    // 结束会话(单字段 UPDATE,不覆盖前面 insert_message 累加的消费字段)
    store.end_session(&session.id, "正常结束").await.unwrap();

    let read_back = store.get(&session.id).await.unwrap().unwrap();
    assert!(read_back.ended_at.is_some(), "结束后 ended_at 应被设置");
    assert_eq!(read_back.end_reason.as_deref(), Some("正常结束"));
    assert!((read_back.total_cost - 0.02).abs() < 1e-9);
    assert_eq!(read_back.total_prompt_tokens, 200);
}
