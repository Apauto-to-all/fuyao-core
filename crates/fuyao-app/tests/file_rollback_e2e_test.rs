//! 文件回退端到端集成测试
//!
//! 在最高接缝（Engine 真实 turn + 真实影子仓 + 真实 SQLite + SessionManager 编排）
//! 跑通「工具批改文件 → 快照落账（含 turn 收尾行）→ 只读预览 → 回退 → 文件与消息
//! 终态一致」全流程：
//! 1. 核心场景：回退刚结束的 turn，该 turn 全部批次（含最后一批）的文件改动完整
//!    回退——修改恢复为回退点内容、新建被删除；执行结论与预览一致；快照行
//!    同谓词清理。
//! 2. 降级路径：快照禁用时工具照常执行、回退仅消息、文件现场不动。
//!
//! 真实性边界：LLM 用脚本化 fake（外部不可控依赖），其余全真实——FileSnapshot 走
//! 真实 git、SessionStore 走真实 SQLite、快照行由引擎真实 turn 落账（不手工插行）。

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use common::{make_store, text_events};
use fuyao_api::message::input::{UserMessage, UserPayload};
use fuyao_api::message::{EventBase, InputEvent, OutputEvent};
use fuyao_api::{AgentConfig, AgentPaths, EngineParams, MessageRole, ModelConfig, SessionParams};
use fuyao_app::{App, FileRollbackOutcome, FilesPreview, LogGuard, SessionManager};
use fuyao_core::{Engine, PluginHost, ToolRegistry};
use fuyao_provider::{
    BoxStream, ChatRequest, ChatResponse, FinishReason, Provider, StreamError, StreamEvent,
    StreamOptions, StreamUsage,
};
use fuyao_snapshot::{DEFAULT_MAX_UNTRACKED_MB, FileSnapshot};
use tokio::time::timeout;

/// 按调用次数依次返回脚本中的事件序列（ReAct 多轮各取一支）
struct ScriptedProvider {
    scripts: Vec<Vec<StreamEvent>>,
    call: AtomicU32,
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn stream_chat(
        &self,
        _request: ChatRequest,
        _model: &str,
        _options: StreamOptions,
    ) -> BoxStream<Result<StreamEvent, StreamError>> {
        let idx = self.call.fetch_add(1, Ordering::SeqCst) as usize;
        let events: Vec<Result<StreamEvent, StreamError>> = self
            .scripts
            .get(idx)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(Ok)
            .collect();
        Box::pin(futures_util::stream::iter(events))
    }

    async fn chat(
        &self,
        _request: ChatRequest,
        _model: &str,
        _options: StreamOptions,
    ) -> Result<ChatResponse, StreamError> {
        // 标题生成走非流式调用：fake 一律失败，标题回退占位（不影响被测链路）
        Err(StreamError::ApiError {
            status: None,
            message: "mock: chat 不支持".into(),
        })
    }
}

/// 构造一次工具调用的流事件序列（单 chunk 全量携带 id+name+args，合法形式）
fn tool_call_events(id: &str, name: &str, args: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCallChunk {
            index: 0,
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            args_delta: Some(args.to_string()),
        },
        StreamEvent::Done {
            usage: StreamUsage::default(),
            finish_reason: FinishReason::ToolCalls,
        },
    ]
}

/// 注册写文件工具：参数 `{"path": 相对路径, "content": 内容}`，写入指定工作区
fn write_file_registry(worktree: std::path::PathBuf) -> ToolRegistry {
    let handler: fuyao_api::ToolFn = Arc::new(move |args, _ctx, _cancel| {
        let target = worktree.clone();
        Box::pin(async move {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let content = args
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            match std::fs::write(target.join(path), content) {
                Ok(()) => fuyao_api::ToolOutput::text(format!("已写入 {path}")),
                Err(cause) => fuyao_api::ToolOutput::text(format!("写入失败：{cause}")),
            }
        })
    });
    let entry = fuyao_api::ToolEntry {
        definition: fuyao_api::ToolDefinition::new("write_file", "向工作区写文件"),
        handler,
        child_invisible: false,
    };
    ToolRegistry::builder().register(entry).build()
}

/// 测试用 SessionParams：`test/model` 与 ProviderRegistry 注册的 provider_id 匹配
fn session_params() -> SessionParams {
    SessionParams {
        agent_config: AgentConfig {
            definition: "default".to_string(),
        },
        model_config: ModelConfig {
            model_id: "test/model".to_string(),
            thinking_type: None,
            reasoning_effort: None,
        },
    }
}

/// 端到端测试台：真实影子仓 + 真实 SQLite + 引擎 + App + SessionManager
///
/// `_home` / `_ws` / `_shadow` 三个 TempDir guard 须存活到测试结束（引擎与快照器
/// 持有其中的路径）。
struct E2eFixture {
    manager: SessionManager,
    app: App,
    store: Arc<fuyao_session::SessionStore>,
    /// 工作区目录（断言文件终态用）
    worktree: std::path::PathBuf,
    _home: tempfile::TempDir,
    _ws: tempfile::TempDir,
    _shadow: tempfile::TempDir,
}

/// 装配带真实影子仓的端到端测试台（快照可用态）
async fn e2e_fixture(scripts: Vec<Vec<StreamEvent>>) -> E2eFixture {
    let home = tempfile::tempdir().expect("创建临时 fuyao_home 失败");
    let ws = tempfile::tempdir().expect("创建临时工作区失败");
    let shadow = tempfile::tempdir().expect("创建临时影子仓目录失败");
    let worktree = ws.path().to_path_buf();
    let snapshot = FileSnapshot::new(&worktree, shadow.path(), DEFAULT_MAX_UNTRACKED_MB).await;
    assert!(snapshot.is_enabled(), "测试前提：git 在 PATH，影子仓可用");

    let agent_paths = AgentPaths {
        agent_id: None,
        workspace: Some(worktree.clone()),
        extra_dirs: Vec::new(),
        fuyao_home: home.path().to_path_buf(),
    };
    let store = make_store(&agent_paths).await;
    let providers = fuyao_provider::ProviderRegistry::with_instance(
        "test",
        Arc::new(ScriptedProvider {
            scripts,
            call: AtomicU32::new(0),
        }),
    );
    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        providers,
        write_file_registry(worktree.clone()),
        PluginHost::new(),
        store.clone(),
        snapshot.clone(),
    )
    .await;
    let app = App::new(engine, None, LogGuard::default());
    let manager = SessionManager::new(store.clone(), snapshot);
    E2eFixture {
        manager,
        app,
        store,
        worktree,
        _home: home,
        _ws: ws,
        _shadow: shadow,
    }
}

/// 构造 Guide 模式用户消息（InputEvent 形态，走 App::send 生产同路径）
fn guide_msg(content: &str) -> InputEvent {
    InputEvent::User(UserMessage {
        base: EventBase::default(),
        payload: UserPayload {
            content: content.to_string(),
            images: vec![],
            mode: Default::default(),
            source: Default::default(),
            client_message_id: None,
        },
    })
}

/// 跑完一个 turn：建会话 → 发用户消息 → 消费事件直到最终回复（finish_reason=stop），
/// 返回 (session_id, 最终回复内容)。
async fn run_turn_to_completion(fx: &E2eFixture, user_content: &str) -> (String, String) {
    let session_id = fx
        .app
        .create_session(session_params())
        .await
        .expect("创建 session 失败");
    fx.app
        .send(&session_id, guide_msg(user_content))
        .await
        .expect("发消息失败");

    // 消费 fan_out 直到最终 Assistant（stop）：单事件 5s 超时防挂死
    let mut final_reply = String::new();
    for _ in 0..400 {
        let Ok(Some(ev)) = timeout(Duration::from_secs(5), fx.app.recv()).await else {
            break;
        };
        if let OutputEvent::Assistant(a) = ev
            && a.payload.finish_reason.as_deref() == Some("stop")
            && a.base.session_id.as_deref() == Some(session_id.as_str())
        {
            final_reply = a.payload.content.unwrap_or_default();
            break;
        }
    }
    assert!(
        !final_reply.is_empty(),
        "应收到最终回复（finish_reason=stop）"
    );
    (session_id, final_reply)
}

/// 等待引擎落满 expected 行快照（最终回复事件先于 turn 收尾补拍到达，轮询作屏障）
async fn wait_snapshot_rows(fx: &E2eFixture, session_id: &str, expected: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let rows = fx
            .store
            .list_file_snapshots_from(session_id, 0)
            .await
            .expect("查快照行失败");
        if rows.len() >= expected {
            assert_eq!(rows.len(), expected, "快照行数应恰为期望值");
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "5 秒内未落满 {expected} 行快照（当前 {} 行）",
            rows.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 取会话里唯一 user 消息的 seq（回退目标）
async fn sole_user_seq(fx: &E2eFixture, session_id: &str) -> i64 {
    let page = fx
        .manager
        .list_messages(session_id, None, None)
        .await
        .expect("查历史失败");
    page.items
        .iter()
        .find(|m| m.role == MessageRole::User)
        .map(|m| m.seq)
        .expect("应有 user 消息")
}

/// 核心场景：回退刚结束的 turn，全部批次（含最后一批）的文件改动完整回退
///
/// turn 结构：批1 改 a.txt（v1→v2）→ 批2 新建 n.txt → 最终回复。快照账 = 批1 边界行
/// + 批2 边界行 + 收尾行（承载终批新建）。回退到本 turn 的 user 消息：基线树 =
/// 批1 执行前现场（a.txt=v1、无 n.txt），触碰集 = 收尾行的 files——a.txt 恢复 v1、
/// n.txt（基线树外）删除；预览与执行结论一致、快照行清理。
#[tokio::test]
async fn rollback_after_completed_turn_reverts_all_batches_including_last() {
    let fx = e2e_fixture(vec![
        tool_call_events(
            "tc_1",
            "write_file",
            r#"{"path":"a.txt","content":"v2 批1修改"}"#,
        ),
        tool_call_events(
            "tc_2",
            "write_file",
            r#"{"path":"n.txt","content":"批2新建"}"#,
        ),
        text_events("全部完成"),
    ])
    .await;
    std::fs::write(fx.worktree.join("a.txt"), "v1").expect("预置 a.txt 失败");

    let (session_id, final_reply) = run_turn_to_completion(&fx, "改文件并新建文件").await;
    assert_eq!(final_reply, "全部完成");

    // 引擎真实落账：两个批边界行 + 一个收尾行（终批变更窗口由收尾行承载）
    wait_snapshot_rows(&fx, &session_id, 3).await;
    assert_eq!(
        std::fs::read_to_string(fx.worktree.join("a.txt")).unwrap(),
        "v2 批1修改",
        "批1 工具效果在盘上"
    );
    assert!(
        fx.worktree.join("n.txt").exists(),
        "终批（批2）新建的文件在盘上"
    );

    // 预览：将删消息 = user 及其后全部 6 条（user / a1 / tool / a2 / tool / a3）；
    // 文件影响 = a.txt 恢复基线内容、n.txt（基线树外）删除
    let target = sole_user_seq(&fx, &session_id).await;
    let preview = fx
        .manager
        .preview_rollback(&session_id, target, true)
        .await
        .expect("预览应成功");
    assert_eq!(preview.messages_to_delete.len(), 6, "user 及其后共 6 条");
    assert_eq!(preview.messages_to_delete[0].seq, target, "首条即目标");
    assert_eq!(
        preview.files,
        FilesPreview::Plan {
            to_restore: vec!["a.txt".to_string()],
            to_delete: vec!["n.txt".to_string()],
        },
        "预览文件影响：触碰集对基线树的分类（含终批新建）"
    );

    // 执行：文件终态与消息终态一致
    let outcome = fx
        .manager
        .rollback_session(&session_id, target, true)
        .await
        .expect("回退应成功");
    assert_eq!(
        outcome,
        FileRollbackOutcome::Restored {
            restored: vec!["a.txt".to_string()],
            deleted: vec!["n.txt".to_string()],
        }
    );
    assert_eq!(
        std::fs::read_to_string(fx.worktree.join("a.txt")).unwrap(),
        "v1",
        "批1 的修改应回到回退点内容"
    );
    assert!(
        !fx.worktree.join("n.txt").exists(),
        "终批（批2）新建的文件应被删除——回退刚结束的 turn 不漏终批"
    );

    // 消息终态：目标及其后全部删除
    let page = fx
        .manager
        .list_messages(&session_id, None, None)
        .await
        .expect("查历史失败");
    assert!(page.items.is_empty(), "回退后历史应为空");
    // 快照行同谓词清理
    let rows = fx
        .store
        .list_file_snapshots_from(&session_id, 0)
        .await
        .expect("查快照行失败");
    assert!(rows.is_empty(), "回退后快照行应全部清理");

    fx.app.shutdown().await;
}

/// 降级路径：快照禁用时工具照常执行、不落快照行；回退降级为仅消息——文件现场
/// 不动、结论明示不可用
#[tokio::test]
async fn disabled_snapshot_rollback_degrades_to_message_only() {
    // 与 e2e_fixture 同构，但快照器为禁用态（enabled = false 的装配分支）
    let home = tempfile::tempdir().expect("创建临时 fuyao_home 失败");
    let ws = tempfile::tempdir().expect("创建临时工作区 失败");
    let worktree = ws.path().to_path_buf();
    std::fs::write(worktree.join("a.txt"), "v1").expect("预置 a.txt 失败");
    let _shadow = tempfile::tempdir().expect("创建临时影子仓目录失败");

    let agent_paths = AgentPaths {
        agent_id: None,
        workspace: Some(worktree.clone()),
        extra_dirs: Vec::new(),
        fuyao_home: home.path().to_path_buf(),
    };
    let store = make_store(&agent_paths).await;
    let providers = fuyao_provider::ProviderRegistry::with_instance(
        "test",
        Arc::new(ScriptedProvider {
            scripts: vec![
                tool_call_events(
                    "tc_1",
                    "write_file",
                    r#"{"path":"a.txt","content":"v2 批1修改"}"#,
                ),
                text_events("完成"),
            ],
            call: AtomicU32::new(0),
        }),
    );
    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        providers,
        write_file_registry(worktree.clone()),
        PluginHost::new(),
        store.clone(),
        FileSnapshot::disabled(),
    )
    .await;
    let app = App::new(engine, None, LogGuard::default());
    let manager = SessionManager::new(store.clone(), FileSnapshot::disabled());
    let fx = E2eFixture {
        manager,
        app,
        store,
        worktree,
        _home: home,
        _ws: ws,
        _shadow,
    };

    let (session_id, _final_reply) = run_turn_to_completion(&fx, "改文件").await;

    // 工具照常执行、零快照行
    assert_eq!(
        std::fs::read_to_string(fx.worktree.join("a.txt")).unwrap(),
        "v2 批1修改",
        "禁用态不影响工具执行"
    );
    let rows = fx
        .store
        .list_file_snapshots_from(&session_id, 0)
        .await
        .expect("查快照行失败");
    assert!(rows.is_empty(), "禁用态不应落任何快照行");

    // 预览：文件侧诚实标注不可用
    let target = sole_user_seq(&fx, &session_id).await;
    let preview = fx
        .manager
        .preview_rollback(&session_id, target, true)
        .await
        .expect("预览应成功");
    assert_eq!(preview.files, FilesPreview::Unavailable);
    assert_eq!(
        preview.messages_to_delete.len(),
        4,
        "消息侧预览照常（user / assistant / tool / assistant）"
    );

    // 执行：仅消息回退、文件现场不动、结论明示不可用
    let outcome = fx
        .manager
        .rollback_session(&session_id, target, true)
        .await
        .expect("消息回退照常");
    assert_eq!(outcome, FileRollbackOutcome::Unavailable);
    assert_eq!(
        std::fs::read_to_string(fx.worktree.join("a.txt")).unwrap(),
        "v2 批1修改",
        "禁用态回退不触碰文件现场"
    );
    let page = fx
        .manager
        .list_messages(&session_id, None, None)
        .await
        .expect("查历史失败");
    assert!(page.items.is_empty(), "消息照常回退");

    fx.app.shutdown().await;
}
