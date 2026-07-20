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
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
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

/// 按调用次数返回不同结果的 Mock：用于测重试场景
///
/// - 第 1..N 次调用 `stream_chat` 返回 `errors[i-1]` 错误流
/// - 第 N+1 次起返回 `events` 事件流
///
/// 例：`FlakyThenSuccessProvider::new(vec![RateLimit], text_events("ok"))`
/// 第 1 次失败 RateLimit，第 2 次成功吐事件。
pub struct FlakyThenSuccessProvider {
    /// 按顺序返出的错误（每次错误流首项即错误本身）
    pub errors: Mutex<Vec<StreamError>>,
    /// 错误耗尽后返回的事件序列
    pub events: Vec<StreamEvent>,
    /// 调用计数（验证重试次数）
    pub calls: AtomicU32,
}

impl FlakyThenSuccessProvider {
    pub fn new(errors: Vec<StreamError>, events: Vec<StreamEvent>) -> Self {
        Self {
            errors: Mutex::new(errors),
            events,
            calls: AtomicU32::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Provider for FlakyThenSuccessProvider {
    fn stream_chat(
        &self,
        _request: ChatRequest,
        _model: &str,
        _options: StreamOptions,
    ) -> BoxStream<Result<StreamEvent, StreamError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);

        // 先看错误队列有没有
        let next_err = {
            let mut guard = self.errors.lock().unwrap();
            if guard.is_empty() {
                None
            } else {
                Some(guard.remove(0))
            }
        };

        match next_err {
            // 错误：只产一个错误项（与 stream::iter 一致：错误后流终止）
            Some(e) => {
                let item: Result<StreamEvent, StreamError> = Err(e);
                Box::pin(stream::iter(std::iter::once(item)))
            }
            // 成功：吐 events
            None => {
                let events: Vec<Result<StreamEvent, StreamError>> =
                    self.events.iter().cloned().map(Ok).collect();
                Box::pin(stream::iter(events))
            }
        }
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
