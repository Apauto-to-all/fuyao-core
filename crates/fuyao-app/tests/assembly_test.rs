//! fuyao-app 集成测试：装配链路
//!
//! 聚焦**装配层专属**行为（不测引擎核心 ReAct / retry / shutdown / end_session——
//! 那些已迁移至 `fuyao-core/tests/`，由 Engine 直连验证）：
//! 1. `build_tool_registry`：内置工具（fuyao-tools 静态表）按 `[tools.enabled]` 注入注册表，
//!    断言核心工具 read/write/glob/grep/bash 存在。
//! 2. `App::shutdown` 对无 MCP manager 的 App 不 panic 且消费 self drop log_guard
//!    （shutdown 串联 engine + MCP 的装配侧收尾，而非 Engine 自身 shutdown 语义）。
//!
//! 全程用临时 fuyao_home（隔离 sessions.db），不调 set_config，走 get_config default 兜底
//! （tools.enabled 空 → 全启用；mcp_servers 空 → 不启动 MCP）。

mod common;

use common::{MockProvider, make_store, temp_agent_paths, text_events};
use fuyao_api::EngineParams;
use fuyao_app::{App, LogGuard, build_tool_registry};
use fuyao_core::{Engine, PluginHost};

/// 把任意 Provider 包成 ProviderRegistry（统一 provider_id="test"）
fn as_providers<P: fuyao_provider::Provider + 'static>(p: P) -> fuyao_provider::ProviderRegistry {
    fuyao_provider::ProviderRegistry::with_instance("test", std::sync::Arc::new(p))
}

// ============================================================================
// build_tool_registry：内置工具注入
// ============================================================================

#[tokio::test]
async fn build_registry_includes_core_tools() {
    let (registry, mcp_manager) = build_tool_registry().await;

    // 无 mcp 配置 → mcp_manager 为 None
    assert!(mcp_manager.is_none(), "无 MCP 配置时 mcp_manager 应为 None");

    let defs = registry.definitions_for(false, &std::collections::HashMap::new());
    assert!(
        defs.len() >= 5,
        "应注入至少 5 个内置工具，实际：{}",
        defs.len()
    );

    // 定义直接携带名称，校验核心工具存在
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    for expected in ["read", "write", "glob", "grep", "bash"] {
        assert!(
            names.contains(&expected),
            "应注入工具 {expected}，实际：{names:?}"
        );
    }
}

// ============================================================================
// App::shutdown 装配侧收尾
// ============================================================================

/// App::shutdown 对无 MCP manager 的 App 不 panic，且消费 self drop log_guard
///
/// 此处验证装配层的收尾串联（engine.shutdown + 无 MCP 跳过 stop_all + forwarder 收尾
/// + self drop），而非 Engine 自身的 shutdown 语义（后者见 fuyao-core/tests/shutdown_test.rs）。
#[tokio::test]
async fn shutdown_with_no_mcp_manager_does_not_panic() {
    let (agent_paths, _home) = temp_agent_paths();
    let store = make_store(&agent_paths).await;
    let engine = Engine::new(
        EngineParams {
            agent_paths: agent_paths.clone(),
        },
        as_providers(MockProvider {
            events: text_events("ok"),
        }),
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
        store,
    )
    .await;

    let app = App::new(engine, None, LogGuard::default());

    // 不应 panic：engine.shutdown() + 无 MCP（跳过 stop_all）+ forwarder 收尾 + self drop
    app.shutdown().await;
}
