//! 子代理工具端到端集成测试
//!
//! 验证完整流程：父 ReAct → LLM 调 subagent 工具 → handler 派生子 session →
//! 子 session 跑完 ReAct → handler 取最终回复回喂 → 父收到 tool_result → 父继续到 stop。
//!
//! 同时验证子代理生命周期事件 + 中间事件透传：
//! - `ChildSession(Started)` / `ChildSession(Ended)` 经父 session 出站通道进 fan_out
//! - 子 session 的 Chunk 等中间事件经父 forwarder 进 fan_out（session_id 标 child）
//!
//! 仅覆盖端到端主干路径：
//! - 递归防护（`definitions_json_for(true)` 排除 subagent）已有单测（`tool_registry.rs`）
//! - 子 session 一次性（end_session 后 rx 返 None）已有单测（`child_session_test.rs`）

mod common;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Once};
use std::time::Duration;

use async_trait::async_trait;
use common::{make_store, temp_agent_paths, text_events};
use futures_util::stream;
use fuyao_api::message::input::{UserMessage, UserPayload};
use fuyao_api::message::output::{ChildSessionOrigin, ChildSessionState};
use fuyao_api::message::{EventBase, InputEvent, OutputEvent};
use fuyao_api::{EngineParams, FuyaoConfig, ModelConfig, ModelRef, SessionParams, set_config};
use fuyao_app::{App, LogGuard, build_tool_registry};
use fuyao_core::{Engine, PluginHost};
use fuyao_provider::{
    BoxStream, ChatRequest, ChatResponse, FinishReason, Provider, StreamError, StreamEvent,
    StreamOptions, StreamUsage,
};
use tokio::time::timeout;

/// 全局配置注入守护：本测试二进制内只 set_config 一次
///
/// 子代理 handler 用 `SessionParams::default()` 派生子 session，依赖全局 `[models.default]`
/// 兜底模型路由。set_config 基于 OnceLock，进程内只能成功一次——用 `Once` 保证多测试并行时
/// 仅一个线程进入 set_config，其余直接放行（get_config 读已设值）。
static CONFIG_GUARD: Once = Once::new();

fn ensure_test_config() {
    CONFIG_GUARD.call_once(|| {
        let mut cfg = FuyaoConfig::default();
        // 与 session_params() 的 model_id 同源：test/model → provider_id="test" / model="model"
        cfg.models.default = Some(ModelRef {
            model: "test/model".to_string(),
            ..Default::default()
        });
        set_config(Arc::new(cfg));
    });
}

/// 按调用次数依次返回脚本中的事件序列
///
/// 第 N 次 `stream_chat` 返回 `scripts[N]`。脚本耗尽返空流（调用方应保证脚本覆盖所有预期调用）。
/// 用于端到端编排：父 turn1（tool_call）→ 子 turn（文本）→ 父 turn2（最终回复）。
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

/// 构造一次 subagent 工具调用的流事件序列
///
/// 单个 ToolCallChunk 同时携带 id+name+args（流式解码器按 index 增量拼接，
/// 单 chunk 全量也是合法形式），紧接 Done(ToolCalls)。
fn subagent_call_events(prompt: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCallChunk {
            index: 0,
            id: Some("call_sub_1".to_string()),
            name: Some("subagent".to_string()),
            args_delta: Some(
                serde_json::json!({
                    "subagent_type": "explore",
                    "description": "端到端测试",
                    "prompt": prompt
                })
                .to_string(),
            ),
        },
        StreamEvent::Done {
            usage: StreamUsage::default(),
            finish_reason: FinishReason::ToolCalls,
        },
    ]
}

/// 构造 Guide 模式用户消息
fn guide_msg(content: &str) -> InputEvent {
    InputEvent::User(UserMessage {
        base: EventBase::default(),
        payload: UserPayload {
            content: content.to_string(),
            images: vec![],
            mode: Default::default(),
            source: Default::default(),
        },
    })
}

/// 测试用 SessionParams：`test/model` 与 MockProvider 注册表匹配
fn session_params() -> SessionParams {
    SessionParams {
        model_config: ModelConfig {
            model_id: Some("test/model".to_string()),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// 把任意 Provider 包成 ProviderRegistry（统一 provider_id="test"）
fn as_providers<P: Provider + 'static>(p: P) -> fuyao_provider::ProviderRegistry {
    fuyao_provider::ProviderRegistry::with_instance("test", Arc::new(p))
}

/// 父 ReAct 调用 subagent 工具，handler 派生子 session 执行，父收到 tool_result 并继续到 stop
///
/// 验证项：
/// 1. 父 ReAct 产出含 `subagent` 的 ToolCall 事件
/// 2. handler 发 `ChildSession(Started)` 事件（携带 child_session_id + parent_session_id + tool_call_id）
/// 3. 子 session 的 Chunk 中间事件被 forward 到 fan_out（session_id 标 child）
/// 4. 父 ReAct 收到含「子代理结果」的 ToolResult 事件
/// 5. handler 发 `ChildSession(Ended)` 事件
/// 6. 父继续下一 turn 产出 finish_reason=stop 的最终 Assistant
#[tokio::test]
async fn parent_react_invokes_subagent_and_receives_tool_result() {
    ensure_test_config();

    let (agent_paths, _home) = temp_agent_paths();
    let (registry, _mcp_manager) = build_tool_registry().await;
    let store = make_store(&agent_paths).await;

    // 脚本按 stream_chat 调用顺序：
    // - [0] 父 turn1：返回 subagent tool_call
    // - [1] 子 turn：返回「子代理结果」文本 + Stop（产生 Chunk 中间事件供 forward）
    // - [2] 父 turn2：收到 tool_result 后返回最终文本 + Stop
    let provider = ScriptedProvider {
        scripts: vec![
            subagent_call_events("做某事"),
            text_events("子代理结果"),
            text_events("父确认收到"),
        ],
        call: AtomicU32::new(0),
    };

    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(provider),
        registry,
        PluginHost::new(),
        store,
    )
    .await;
    let app = App::new(engine, None, LogGuard::default());

    let parent_id = app
        .create_session(session_params())
        .await
        .expect("创建父 session 失败");
    app.send(&parent_id, guide_msg("派子代理做某事"))
        .await
        .expect("发消息失败");

    // 消费 fan_out：按事件类型断言关键节点
    let mut got_tool_call = false;
    let mut got_child_started = false;
    let mut got_child_chunk = false;
    let mut got_tool_result_with_child_content = false;
    let mut got_child_ended = false;
    let mut got_final_assistant = false;
    let mut child_session_id = String::new();

    for _ in 0..400 {
        // 单事件 5s 超时：脚本正确时毫秒级产出，5s 足够开发机抖动
        let Ok(Some(ev)) = timeout(Duration::from_secs(5), app.recv()).await else {
            break;
        };
        match ev {
            OutputEvent::ToolCall(tc) => {
                assert_eq!(
                    tc.payload.tool_name, "subagent",
                    "ToolCall 应是 subagent 工具"
                );
                got_tool_call = true;
            }
            OutputEvent::ChildSession(cs) => {
                assert_eq!(
                    cs.payload.parent_session_id, parent_id,
                    "ChildSession 事件的 parent_session_id 应是父 session"
                );
                assert_eq!(
                    cs.payload.origin,
                    ChildSessionOrigin::Subagent,
                    "origin 应是 Subagent"
                );
                assert_eq!(
                    cs.payload.tool_call_id.as_deref(),
                    Some("call_sub_1"),
                    "tool_call_id 应是触发本次派生的 LLM tool_call id"
                );
                child_session_id = cs.payload.child_session_id.clone();
                match cs.payload.state {
                    ChildSessionState::Started => got_child_started = true,
                    ChildSessionState::Ended => got_child_ended = true,
                }
            }
            OutputEvent::Chunk(c) => {
                // 子代理的 Chunk 经 forward 进 fan_out——session_id 应是 child
                if c.base.session_id.as_deref() == Some(child_session_id.as_str())
                    && !child_session_id.is_empty()
                {
                    got_child_chunk = true;
                }
            }
            OutputEvent::ToolResult(tr) => {
                assert_eq!(
                    tr.payload.tool_name, "subagent",
                    "ToolResult 应来自 subagent 工具"
                );
                if tr.payload.content.contains("子代理结果") {
                    got_tool_result_with_child_content = true;
                }
            }
            // 含 tool_calls 的 Assistant 是 turn1 的尾事件，跳过；
            // 只要 finish_reason=stop + 内容含「父确认收到」即父的最终回复
            OutputEvent::Assistant(a)
                if a.payload.finish_reason.as_deref() == Some("stop")
                    && a.payload
                        .content
                        .as_deref()
                        .unwrap_or_default()
                        .contains("父确认收到") =>
            {
                got_final_assistant = true;
                break;
            }
            _ => {}
        }
    }

    assert!(got_tool_call, "父应产出 subagent 的 ToolCall 事件");
    assert!(
        got_child_started,
        "应收到 ChildSession(Started) 事件（携带 child_session_id）"
    );
    assert!(
        !child_session_id.is_empty(),
        "应从 ChildSession 事件提取 child_session_id"
    );
    assert!(
        got_child_chunk,
        "子代理的 Chunk 中间事件应被 forward 到 fan_out"
    );
    assert!(
        got_tool_result_with_child_content,
        "父应收到含子代理最终回复的 ToolResult"
    );
    assert!(got_child_ended, "应收到 ChildSession(Ended) 事件");
    assert!(
        got_final_assistant,
        "父应在收到 tool_result 后继续 ReAct 产出最终 Assistant"
    );

    app.shutdown().await;
}
