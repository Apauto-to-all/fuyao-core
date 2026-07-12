//! fuyao-app 集成测试共享 fixture
//!
//! MockProvider（复用 fuyao-core/tests/common 的 stream::iter 模式）+
//! AgentContext 构造（注入临时 fuyao_home，避免 SessionPlugin 触碰真实 ~/.fuyao）。

use futures_util::stream;
use fuyao_api::{AgentContext, AgentPaths};
use fuyao_provider::{
    BoxStream, ChatRequest, ChatResponse, FinishReason, Provider, StreamError, StreamEvent,
    StreamOptions, StreamUsage,
};
use std::path::PathBuf;
use std::sync::Arc;
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

/// 构造含临时 fuyao_home 的 AgentContext
///
/// 临时 fuyao_home 隔离 SessionPlugin 的 sessions.db 落点，不触碰真实 ~/.fuyao。
/// 返回 (ctx, _home_guard)：guard 须存活到测试结束以保住临时目录。
pub fn test_agent_ctx_with_temp_home() -> (AgentContext, TempDir) {
    let home = tempfile::tempdir().expect("创建临时 fuyao_home 失败");
    let ctx = AgentContext {
        model_config: fuyao_api::ModelConfig {
            model_id: Some("test-model".to_string()),
            ..Default::default()
        },
        agent_paths: AgentPaths {
            agent_id: None,
            workspace: None,
            extra_dirs: Vec::new(),
            fuyao_home: home.path().to_path_buf(),
        },
        ..Default::default()
    };
    (ctx, home)
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

/// 构造带临时 fuyao_home 的 AgentPaths（独立于 AgentContext，供需要 paths 的场景）
#[allow(dead_code)]
pub fn temp_agent_paths(fuyao_home: PathBuf) -> AgentPaths {
    AgentPaths {
        agent_id: None,
        workspace: None,
        extra_dirs: Vec::new(),
        fuyao_home,
    }
}

/// 抑制未使用 Arc 警告（test_agent_ctx_with_temp_home 不返回 Arc，但保留通用性）
#[allow(dead_code)]
fn _arc_use() {
    let _: Arc<()> = Arc::new(());
}
