//! fuyao-app 集成测试共享 fixture
//!
//! MockProvider（复用 stream::iter 模式）+ 临时 AgentPaths 构造
//! （注入临时 fuyao_home，避免触碰真实 ~/.fuyao）。
//!
//! 文件级 `#![allow(dead_code)]`：本文件被多个测试二进制共享（每个测试文件独立编译），
//! 不同二进制用到不同子集 fixture，未用部分会触发 dead_code 警告——属于共享 fixture 的正常现象。

#![allow(dead_code)]

use futures_util::stream;
use fuyao_api::AgentPaths;
use fuyao_provider::{
    BoxStream, ChatRequest, ChatResponse, FinishReason, Provider, StreamError, StreamEvent,
    StreamOptions, StreamUsage,
};
use tempfile::TempDir;

/// 最简 MockProvider：固定事件序列（每次 stream_chat 返回同一序列）
pub struct MockProvider {
    pub events: Vec<StreamEvent>,
}

#[async_trait::async_trait]
impl Provider for MockProvider {
    fn stream_chat(
        &self,
        _request: ChatRequest,
        _model: &str,
        _options: StreamOptions,
    ) -> BoxStream<Result<StreamEvent, StreamError>> {
        let events: Vec<Result<StreamEvent, StreamError>> =
            self.events.iter().cloned().map(Ok).collect();
        Box::pin(stream::iter(events))
    }

    async fn chat(
        &self,
        _request: ChatRequest,
        _model: &str,
        _options: StreamOptions,
    ) -> Result<ChatResponse, StreamError> {
        Err(StreamError::ApiError("mock: chat 不支持".into()))
    }
}

/// 构造带临时 fuyao_home 的 AgentPaths
///
/// 临时 fuyao_home 隔离 sessions.db 落点，不触碰真实 ~/.fuyao。
/// 返回 (paths, _home_guard)：guard 须存活到测试结束以保住临时目录。
pub fn temp_agent_paths() -> (AgentPaths, TempDir) {
    let home = tempfile::tempdir().expect("创建临时 fuyao_home 失败");
    let paths = AgentPaths {
        agent_id: None,
        workspace: None,
        extra_dirs: Vec::new(),
        fuyao_home: home.path().to_path_buf(),
    };
    (paths, home)
}

/// 由 agent_paths 创建会话存储（Arc 包裹，可直接注入 Engine::new）
///
/// store 所有权归调用方（装配方），创建后注入 `Engine::new` 与可能的 SessionManager。
pub async fn make_store(agent_paths: &AgentPaths) -> std::sync::Arc<fuyao_session::SessionStore> {
    let db_path = agent_paths.sessions_db_path();
    std::sync::Arc::new(
        fuyao_session::SessionStore::new(db_path)
            .await
            .expect("创建会话存储失败"),
    )
}

/// 构造纯文本回复事件序列
pub fn text_events(content: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta {
            content: content.to_string(),
        },
        StreamEvent::Done {
            usage: StreamUsage::default(),
            finish_reason: FinishReason::Stop,
        },
    ]
}
