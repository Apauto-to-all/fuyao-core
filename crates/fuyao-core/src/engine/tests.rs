//! fork / 子任务 session 创建与 stop_session 停止原语的语义单元测试
//!
//! fork / 子任务部分覆盖 parent_session_id 在三种创建路径下的正确设置：
//! - fork_session（纯 fork）：parent = None
//! - create_child_session 的 Fork 模式：parent = 父 id
//! - create_child_session 的 Fresh 模式：parent = 父 id
//!
//! 前两个直接测 [`Engine::build_forked_session`]（仅做 DB 层加载 + 复制，
//! 不 assemble / 不 spawn task）；Fresh 模式测公开 [`Engine::create_child_session`]
//! （会 spawn 一个 idle session task，测试末尾用 end_session 收尾拆除）。
//!
//! stop_session 部分覆盖其三段语义：未挂载幂等（Ok）、idle 幂等（Ok）、
//! 屏障语义（返回即 turn 已终止且中断收尾落库完毕）。

use super::*;
use fuyao_api::{
    AgentConfig, AgentPaths, EngineParams, Message, ModelConfig, Session, SessionParams,
};
use fuyao_hooks::PluginHost;
use fuyao_provider::ProviderRegistry;

/// 构造最小可用 Engine（空 provider / 工具 / 插件 + 临时 db 隔离）
///
/// 返回 `(Engine, TempDir)`：TempDir 由调用方持有，存活到测试结束自动清理，
/// 保证 SessionStore 的 db 路径在测试期间有效（无需 leak）。
async fn make_engine() -> (Arc<Engine>, tempfile::TempDir) {
    make_engine_with(ProviderRegistry::default()).await
}

/// 同 [`make_engine`]，但注入自定义 Provider 注册表（stop_session 等需要真实
/// 流式行为的测试用）
async fn make_engine_with(providers: ProviderRegistry) -> (Arc<Engine>, tempfile::TempDir) {
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
        providers,
        ToolRegistry::builder().build(),
        PluginHost::new(),
        store,
    )
    .await;
    (engine, dir)
}

/// 在 store 里建一个带 2 条普通消息的源 session，返回其 id
async fn seed_source_session(engine: &Engine) -> SessionId {
    let source = Session::new(None, None, Some("源系统提示词".to_string()));
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

    // 落库后读回一致：parent None + 可见消息复制到位 + 计数对齐
    // message_count 由 insert_message 事务内累加进 sessions 表，
    // 内存返回值不回读——一律从 DB 行验证
    let loaded = engine.store.get(&new.id).await.unwrap().unwrap();
    assert!(loaded.parent_session_id.is_none());
    assert_eq!(
        loaded.message_count, 2,
        "DB message_count 应对齐复制消息条数"
    );
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
    // 先建父 session：create_child_session 从父继承 model_config，父必须在调度表
    let parent_id = engine
        .create_session(SessionParams {
            agent_config: AgentConfig {
                definition: "default".to_string(),
            },
            model_config: ModelConfig {
                model_id: "test/model".to_string(),
                thinking_type: None,
                reasoning_effort: None,
            },
        })
        .await
        .unwrap()
        .0;

    let (child_id, _rx) = engine
        .create_child_session(
            &parent_id,
            ChildSessionSource::Fresh,
            AgentConfig {
                definition: "explore".to_string(),
            },
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

// ===== stop_session：停止原语的三段语义 =====

/// 中途挂起的流式 Provider：吐一个 TextDelta 后永久挂起（不发 Done、流不结束）
///
/// 让 turn 稳定停在流式阶段——流式 select! 监听中断通道，是验证 stop_session
/// 屏障语义的理想卡点：停止信号送达 → 中断收尾（部分 AssistantMessage 落库）→
/// turn 终止 → 相位回 Idle → stop_session 返回。
struct HangingMidStreamProvider;

#[async_trait::async_trait]
impl fuyao_provider::Provider for HangingMidStreamProvider {
    fn stream_chat(
        &self,
        _request: fuyao_provider::ChatRequest,
        _model: &str,
        _options: fuyao_provider::StreamOptions,
    ) -> fuyao_provider::BoxStream<Result<fuyao_provider::StreamEvent, fuyao_provider::StreamError>>
    {
        let s = async_stream::stream! {
            yield Ok(fuyao_provider::StreamEvent::TextDelta {
                content: "部分输出".to_string(),
            });
            // 永久挂起：不发 Done，流不结束（turn 卡在流式阶段等中断）
            futures_util::future::pending::<()>().await;
        };
        Box::pin(s)
    }

    async fn chat(
        &self,
        _request: fuyao_provider::ChatRequest,
        _model: &str,
        _options: fuyao_provider::StreamOptions,
    ) -> Result<fuyao_provider::ChatResponse, fuyao_provider::StreamError> {
        Err(fuyao_provider::StreamError::ApiError {
            status: None,
            message: "mock: chat 不支持".into(),
        })
    }
}

/// 构造测试用 SessionParams（model_id 指向 make_engine_with 注册的 "test" provider）
fn engine_test_params() -> SessionParams {
    SessionParams {
        agent_config: AgentConfig {
            definition: "default".to_string(),
        },
        model_config: ModelConfig {
            model_id: "test/test-model".to_string(),
            thinking_type: None,
            reasoning_effort: None,
        },
    }
}

/// 未挂载的 session：无 task 即无 DB 写入者，静默前提平凡成立 → Ok（幂等）
#[tokio::test]
async fn stop_session_unmounted_session_returns_ok() {
    let (engine, _dir) = make_engine().await;

    engine
        .stop_session(&"nonexistent".to_string(), "测试停止")
        .await
        .expect("未挂载 session 的停止应幂等返回 Ok");
}

/// idle session（已挂载但无 turn 在跑）：相位即 Idle → Ok（幂等，二次调用同）
#[tokio::test]
async fn stop_session_idle_session_returns_ok_idempotently() {
    let (engine, _dir) = make_engine().await;
    let (id, _rx) = engine.create_session(engine_test_params()).await.unwrap();

    engine.stop_session(&id, "测试停止").await.unwrap();
    engine.stop_session(&id, "再次停止").await.unwrap();

    // 收尾：拆除 session task
    let _ = engine.end_session(&id, "测试结束").await;
}

/// 屏障语义：turn 卡在流式阶段时 stop_session 返回，代表 turn 已完全终止且
/// 中断收尾（部分 AssistantMessage）已落库——返回后 DB 即静默，后续可安全做
/// 存储层写操作（回退等）
#[tokio::test]
async fn stop_session_returns_after_turn_finalization_persisted() {
    let (engine, _dir) = make_engine_with(ProviderRegistry::with_instance(
        "test",
        Arc::new(HangingMidStreamProvider),
    ))
    .await;
    let (id, mut rx_event) = engine.create_session(engine_test_params()).await.unwrap();

    // 发送用户消息，等 turn 进入流式阶段（等到 Chunk 事件即证明）
    engine
        .send(
            &id,
            InputEvent::User(fuyao_api::message::input::UserMessage {
                base: fuyao_api::message::EventBase::default(),
                payload: fuyao_api::message::input::UserPayload {
                    content: "用户问题".to_string(),
                    images: vec![],
                    mode: fuyao_api::UserMessageMode::Guide,
                    source: fuyao_api::message::input::UserMessageSource::User,
                },
            }),
        )
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let ev = tokio::time::timeout_at(deadline, rx_event.recv())
            .await
            .expect("等待进入流式阶段超时")
            .expect("session 事件通道不应提前关闭");
        if matches!(ev, OutputEvent::Chunk(_)) {
            break;
        }
    }

    // 停止：返回即屏障达成（若屏障失效——只发信号不等终止——此处会立刻返回
    // 而 DB 里没有中断收尾的 assistant，下方断言捕获该失败模式）
    engine.stop_session(&id, "测试停止").await.unwrap();

    // 屏障断言：中断收尾的部分 AssistantMessage 已在 DB（user + interrupted assistant）
    let visible = engine.store.load_visible_messages(&id).await.unwrap();
    assert_eq!(visible.len(), 2, "应有 user + 中断收尾的 assistant 两条");
    assert_eq!(visible[1].content.as_deref(), Some("部分输出"));
    assert_eq!(visible[1].finish_reason.as_deref(), Some("interrupted"));

    // 收尾：拆除 session task
    let _ = engine.end_session(&id, "测试结束").await;
}
