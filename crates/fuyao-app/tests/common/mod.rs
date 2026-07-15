//! fuyao-app 集成测试共享 fixture
//!
//! MockProvider（复用 fuyao-core 的 stream::iter 模式）+ 临时 AgentPaths 构造
//! （注入临时 fuyao_home，避免触碰真实 ~/.fuyao）。

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

    async fn chat(&self, _request: ChatRequest, _model: &str) -> Result<ChatResponse, StreamError> {
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
