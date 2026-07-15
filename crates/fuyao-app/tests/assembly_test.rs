//! fuyao-app 集成测试：装配链路
//!
//! 两个聚焦点：
//! 1. `build_tool_registry`：内置工具（fuyao-tools 静态表）按 `[tools.enabled]` 注入注册表，
//!    断言核心工具 read/write/glob/grep/bash 存在。
//! 2. 最小端到端：MockProvider 驱动引擎跑一轮 ReAct（create_session → send → recv 验证
//!    Chunk + Assistant），证明装配产物可端到端运行。
//!
//! 全程用临时 fuyao_home（隔离 sessions.db），不调 set_config，走 get_config default 兜底
//! （tools.enabled 空 → 全启用；mcp_servers 空 → 不启动 MCP）。

mod common;

use std::time::Duration;

use common::{MockProvider, temp_agent_paths, text_events};
use fuyao_api::message::input::{UserMessage, UserPayload};
use fuyao_api::message::{EventBase, InputEvent, OutputEvent};
use fuyao_api::{EngineParams, MessageParams, ModelConfig, SessionParams};
use fuyao_app::build_tool_registry;
use fuyao_core::Engine;

/// 构造 Guide 模式的用户消息
fn guide_user_message(content: &str, model_id: &str) -> (InputEvent, MessageParams) {
    let event = InputEvent::User(UserMessage {
        base: EventBase::default(),
        payload: UserPayload {
            content: content.to_string(),
            mode: Default::default(),
            source: Default::default(),
        },
    });
    let params = MessageParams {
        model_config: ModelConfig {
            model_id: Some(model_id.to_string()),
            ..Default::default()
        },
    };
    (event, params)
}

// ============================================================================
// build_tool_registry：内置工具注入
// ============================================================================

#[tokio::test]
async fn build_registry_includes_core_tools() {
    let (registry, mcp_manager) = build_tool_registry().await;

    // 无 mcp 配置 → mcp_manager 为 None
    assert!(mcp_manager.is_none(), "无 MCP 配置时 mcp_manager 应为 None");

    let defs = registry.definitions_json();
    assert!(
        defs.len() >= 5,
        "应注入至少 5 个内置工具，实际：{}",
        defs.len()
    );

    // 序列化后含 function.name，校验核心工具存在
    let names: Vec<String> = defs
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(String::from)
        })
        .collect();
    for expected in ["read", "write", "glob", "grep", "bash"] {
        assert!(
            names.iter().any(|n| n == expected),
            "应注入工具 {expected}，实际：{names:?}"
        );
    }
}

// ============================================================================
// 端到端：Engine::new + build_tool_registry 后可跑 ReAct
// ============================================================================

#[tokio::test]
async fn assembled_engine_runs_react_loop() {
    let (agent_paths, _home) = temp_agent_paths();
    let (registry, _mcp_manager) = build_tool_registry().await;

    let provider = std::sync::Arc::new(MockProvider {
        events: text_events("装配后回复"),
    }) as std::sync::Arc<dyn fuyao_provider::Provider>;

    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        provider,
        registry,
    )
    .await;

    // 创建 session
    let session_id = engine
        .create_session(SessionParams::default())
        .await
        .expect("创建 session 失败");

    // 发一条 guide 消息（触发一轮 ReAct）
    let (event, params) = guide_user_message("装配后测试", "test/model");
    engine
        .send(&session_id, event, params)
        .await
        .expect("发消息失败");

    // 收事件，验证装配后引擎可产出 Chunk + Assistant
    let mut got_chunk = false;
    let mut got_assistant = false;
    for _ in 0..20 {
        match tokio::time::timeout(Duration::from_millis(2000), engine.recv()).await {
            Ok(Some(e)) => match e {
                OutputEvent::Chunk(_) => got_chunk = true,
                OutputEvent::Assistant(a) => {
                    got_assistant = true;
                    assert!(
                        a.payload
                            .content
                            .as_deref()
                            .unwrap_or_default()
                            .contains("装配后回复"),
                        "Assistant 应含装配后回复内容"
                    );
                }
                _ => {}
            },
            _ => break,
        }
        if got_chunk && got_assistant {
            break;
        }
    }

    engine.shutdown().await;

    assert!(got_chunk, "装配后应产出 Chunk 事件");
    assert!(got_assistant, "装配后应产出 Assistant 事件");
}
