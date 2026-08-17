//! fuyao-core 集成测试共享 fixture
//!
//! MockProvider / FlakyThenSuccessProvider（手写 fake，复用 stream::iter 模式）+
//! 临时 AgentPaths 构造（注入临时 fuyao_home，避免触碰真实 ~/.fuyao）。
//!
//! 与 fuyao-app/tests/common 同源：Provider 的流式 trait automock 无法生成，手写 fake
//! 是项目默认首选（mock 决策树）。文件级 `#![allow(dead_code)]`：本文件被多个测试二进制
//! 共享（每个测试文件独立编译），不同二进制用到不同子集 fixture，未用部分会触发
//! dead_code 警告——属于共享 fixture 的正常现象。

#![allow(dead_code)]

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

    async fn chat(
        &self,
        _request: ChatRequest,
        _model: &str,
        _options: StreamOptions,
    ) -> Result<ChatResponse, StreamError> {
        Err(StreamError::ApiError {
            status: None,
            message: "mock: chat 不支持".into(),
        })
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

    async fn chat(
        &self,
        _request: ChatRequest,
        _model: &str,
        _options: StreamOptions,
    ) -> Result<ChatResponse, StreamError> {
        Err(StreamError::ApiError {
            status: None,
            message: "mock: chat 不支持".into(),
        })
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

// ===== 插件 dispose 生命周期测试 fixture =====

use fuyao_core::{Plugin, PluginInstance, SessionSender};
use std::sync::Arc;

/// 记录 dispose 调用的测试插件（手写 fake：实现两层 trait 记名，不注册任何钩子）
///
/// 实例销毁写 `instance:{name}`、工厂销毁写 `factory:{name}` 进共享流水，
/// 用于断言销毁顺序（后注册的先销毁）与时机（实例 dispose 先于工厂 dispose）。
pub struct DisposeRecordingPlugin {
    /// 插件名（流水条目的标识）
    pub name: &'static str,
    /// dispose 事件流水（实例 + 工厂共用一份，按调用顺序记录）
    pub log: Arc<Mutex<Vec<String>>>,
}

impl Plugin for DisposeRecordingPlugin {
    fn name(&self) -> &str {
        self.name
    }

    fn create_instance(&self) -> Box<dyn PluginInstance> {
        Box::new(DisposeRecordingInstance {
            name: self.name,
            log: Arc::clone(&self.log),
        })
    }

    fn dispose(&self) {
        self.log
            .lock()
            .unwrap()
            .push(format!("factory:{}", self.name));
    }
}

/// [`DisposeRecordingPlugin`] 的 session 实例：不注册钩子，只在 dispose 时记名
pub struct DisposeRecordingInstance {
    /// 插件名（流水条目的标识）
    pub name: &'static str,
    /// dispose 事件流水（与工厂共享同一份）
    pub log: Arc<Mutex<Vec<String>>>,
}

impl PluginInstance for DisposeRecordingInstance {
    fn register(&self, _hooks: &mut fuyao_hooks::HooksRegistry, _sender: &SessionSender) {}

    fn dispose(&self) {
        self.log
            .lock()
            .unwrap()
            .push(format!("instance:{}", self.name));
    }
}
