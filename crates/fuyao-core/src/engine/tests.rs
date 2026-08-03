//! fork / 子任务 session 创建的语义单元测试
//!
//! 覆盖 parent_session_id 在三种创建路径下的正确设置：
//! - fork_session（纯 fork）：parent = None
//! - create_child_session 的 Fork 模式：parent = 父 id
//! - create_child_session 的 Fresh 模式：parent = 父 id
//!
//! 前两个直接测 [`Engine::build_forked_session`]（仅做 DB 层加载 + 复制，
//! 不 assemble / 不 spawn task）；Fresh 模式测公开 [`Engine::create_child_session`]
//! （会 spawn 一个 idle session task，测试末尾用 end_session 收尾拆除）。

use super::*;
use fuyao_api::{AgentPaths, EngineParams, Message, Session, SessionParams};
use fuyao_hooks::PluginHost;
use fuyao_provider::ProviderRegistry;

/// 构造最小可用 Engine（空 provider / 工具 / 插件 + 临时 db 隔离）
///
/// 返回 `(Engine, TempDir)`：TempDir 由调用方持有，存活到测试结束自动清理，
/// 保证 SessionStore 的 db 路径在测试期间有效（无需 leak）。
async fn make_engine() -> (Arc<Engine>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("创建临时目录失败");
    let fuyao_home = dir.path().to_path_buf();
    let agent_paths = AgentPaths {
        agent_id: None,
        workspace: None,
        extra_dirs: Vec::new(),
        fuyao_home,
    };
    // store 所有权归装配方（测试），创建后注入 Engine
    let db_path = agent_paths.sessions_db_path();
    let store = Arc::new(SessionStore::new(db_path).await.expect("创建会话存储失败"));
    let engine = Engine::new(
        EngineParams { agent_paths },
        ProviderRegistry::default(),
        ToolRegistry::builder().build(),
        PluginHost::new(),
        store,
    )
    .await;
    (engine, dir)
}

/// 在 store 里建一个带 2 条普通消息的源 session，返回其 id
async fn seed_source_session(engine: &Engine) -> SessionId {
    let source = Session::new(None, Some("源系统提示词".to_string()));
    engine.store.create(&source).await.unwrap();
    let mut m1 = Message::user("源消息1".to_string());
    engine
        .store
        .insert_message(&source.id, &mut m1)
        .await
        .unwrap();
    let mut m2 = Message::assistant(Some("源回复".to_string()));
    engine
        .store
        .insert_message(&source.id, &mut m2)
        .await
        .unwrap();
    source.id
}

#[tokio::test]
async fn build_forked_session_pure_fork_parent_is_none() {
    // fork_session 路径：build_forked_session(parent=None) → 独立 session（parent 为 None）
    let (engine, _dir) = make_engine().await;
    let source_id = seed_source_session(&engine).await;

    let new = engine.build_forked_session(&source_id, None).await.unwrap();

    // parent_session_id 为 None（独立 session，非子任务）
    assert!(new.parent_session_id.is_none());
    // system_prompt 复制源值
    assert_eq!(new.system_prompt.as_deref(), Some("源系统提示词"));
    // message_count 对齐普通消息条数（2 条，排除 compaction 边界）
    assert_eq!(new.message_count, 2);

    // 落库后读回一致：parent None + 可见消息复制到位
    let loaded = engine.store.get(&new.id).await.unwrap().unwrap();
    assert!(loaded.parent_session_id.is_none());
    let visible = engine.store.load_visible_messages(&new.id).await.unwrap();
    assert_eq!(visible.len(), 2);
    assert_eq!(visible[0].content.as_deref(), Some("源消息1"));
    assert_eq!(visible[1].content.as_deref(), Some("源回复"));
}

#[tokio::test]
async fn build_forked_session_child_fork_parent_is_set() {
    // create_child_session 的 Fork 模式路径：build_forked_session(parent=Some) → parent 为父 id
    let (engine, _dir) = make_engine().await;
    let source_id = seed_source_session(&engine).await;

    let parent_id = "parent-xyz".to_string();
    let new = engine
        .build_forked_session(&source_id, Some(parent_id.clone()))
        .await
        .unwrap();

    // parent_session_id 标记为父 id（子任务 session）
    assert_eq!(new.parent_session_id.as_deref(), Some(parent_id.as_str()));
    // 可见消息复制到位
    let visible = engine.store.load_visible_messages(&new.id).await.unwrap();
    assert_eq!(visible.len(), 2);

    // 落库后 parent 一致
    let loaded = engine.store.get(&new.id).await.unwrap().unwrap();
    assert_eq!(
        loaded.parent_session_id.as_deref(),
        Some(parent_id.as_str())
    );
}

#[tokio::test]
async fn build_forked_session_missing_source_returns_not_found() {
    // 源 session 不存在 → SessionNotFound（而非 Storage / panic）
    let (engine, _dir) = make_engine().await;

    let err = engine
        .build_forked_session(&"nonexistent".to_string(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::SessionNotFound(_)));
}

#[tokio::test]
async fn create_child_session_fresh_sets_parent() {
    // create_child_session 的 Fresh 模式：空上下文，parent 标记为父 id
    let (engine, _dir) = make_engine().await;
    let parent_id = "parent-main".to_string();

    let (child_id, _rx) = engine
        .create_child_session(
            &parent_id,
            ChildSessionSource::Fresh,
            SessionParams::default(),
        )
        .await
        .unwrap();

    // 落库后 parent_session_id 为父 id
    let loaded = engine.store.get(&child_id).await.unwrap().unwrap();
    assert_eq!(
        loaded.parent_session_id.as_deref(),
        Some(parent_id.as_str())
    );
    // 全新创建：空上下文 → message_count = 0
    assert_eq!(loaded.message_count, 0);
    // system_prompt 已从 agent_config 构建（非空，至少含环境 section）
    assert!(
        loaded
            .system_prompt
            .as_deref()
            .is_some_and(|p| !p.is_empty())
    );

    // 收尾：拆除 create_child_session spawn 出来的 idle session task
    let _ = engine.end_session(&child_id, "测试结束").await;
}
