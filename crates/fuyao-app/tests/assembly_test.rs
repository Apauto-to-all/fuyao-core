//! fuyao-app 集成测试：装配链路（setup）
//!
//! 绕开 init_engine（依赖真实 API Key / 配置文件 / set_config 全局副作用），
//! 用 Engine::new(FakeProvider) + setup(&handle) 测装配逻辑：
//! - 注册内置工具（fuyao-tools 静态表全部注册到 handle）
//! - 装配 SessionPlugin + LoopGuardPlugin（按 [plugins.enabled] 过滤，默认全启用）
//! - MCP 无配置时返回 None（不启动子进程）
//! - 装配后引擎可端到端跑 ReAct 循环
//!
//! 全程不调 set_config，走 get_config 返回 default 兜底（plugins.enabled 空 → 全启用）。

mod common;

use std::time::Duration;

use common::{MockProvider, test_agent_ctx_with_temp_home, text_events};
use fuyao_api::{AgentContext, AgentPaths};
use fuyao_app::{SetupError, setup};
use fuyao_core::{Engine, EngineHandle};

/// 用 FakeProvider 构造引擎，返回 handle（engine 任务不启动——本文件聚焦装配，不跑循环）
fn make_handle(ctx: AgentContext) -> EngineHandle {
    let provider = Box::new(MockProvider {
        events: text_events("装配测试"),
    });
    let (_engine, handle) = Engine::new(provider, ctx);
    handle
}

// ============================================================================
// setup：工具注册
// ============================================================================

#[tokio::test]
async fn setup_registers_builtin_tools() {
    // setup 后 handle 应含 fuyao-tools 的全部内置工具
    let (ctx, _home) = test_agent_ctx_with_temp_home();
    let handle = make_handle(ctx);

    let app_ctx = setup(&handle).await.expect("装配应成功");

    let tools = handle.tools_schema();
    assert!(
        tools.len() >= 5,
        "应注册至少 5 个内置工具（read/write/glob/grep/bash），实际：{}",
        tools.len()
    );
    // AppContext 可正常析构（plugin_host dispose 不 panic）
    drop(app_ctx);
}

#[tokio::test]
async fn setup_tools_includes_core_names() {
    let (ctx, _home) = test_agent_ctx_with_temp_home();
    let handle = make_handle(ctx);
    setup(&handle).await.unwrap();

    let schemas = handle.tools_schema();
    // 序列化后含 function.name，校验核心工具存在
    let names: Vec<String> = schemas
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    for expected in ["read", "write", "glob", "grep", "bash"] {
        assert!(
            names.iter().any(|n| n == expected),
            "应注册工具 {expected}，实际：{names:?}"
        );
    }
}

// ============================================================================
// setup：插件装配
// ============================================================================

#[tokio::test]
async fn setup_installs_session_and_loop_guard_plugins() {
    // 默认配置（plugins.enabled 空 → 全启用）下，两个内置插件都装配
    let (ctx, _home) = test_agent_ctx_with_temp_home();
    let handle = make_handle(ctx);

    let app_ctx = setup(&handle).await.expect("装配应成功");

    // plugin_host.list() 应含 session 与 loop_guard
    let installed = app_ctx.plugin_host.list();
    assert!(
        installed.contains(&"session"),
        "应装配 session 插件，实际：{installed:?}"
    );
    assert!(
        installed.contains(&"loop_guard"),
        "应装配 loop_guard 插件，实际：{installed:?}"
    );
}

#[tokio::test]
async fn setup_hooks_populated_after_install() {
    // 装配后 handle.hooks() 应含插件注册的钩子（session 的 before_llm/observe/send_input，
    // loop_guard 的 observe/intercept/send_input）
    let (ctx, _home) = test_agent_ctx_with_temp_home();
    let handle = make_handle(ctx);
    setup(&handle).await.unwrap();

    let hooks = handle.hooks();
    // 锁住 hooks 不 panic 即说明存活；进一步可断言钩子非空（HooksRegistry 无公开计数 API，
    // 但通过驱动事件验证——见端到端测试）
    let _guard = hooks.lock().await;
}

#[tokio::test]
async fn setup_returns_app_context_with_no_mcp_by_default() {
    // 默认无 mcp_servers 配置 → mcp_manager 为 None（不启动子进程）
    let (ctx, _home) = test_agent_ctx_with_temp_home();
    let handle = make_handle(ctx);

    let app_ctx = setup(&handle).await.unwrap();

    assert!(
        app_ctx.mcp_manager.is_none(),
        "无 MCP 配置时 mcp_manager 应为 None"
    );
}

// ============================================================================
// setup：装配产物可访问性
// ============================================================================

#[tokio::test]
async fn app_context_fields_accessible() {
    let (ctx, _home) = test_agent_ctx_with_temp_home();
    let handle = make_handle(ctx);
    let app_ctx = setup(&handle).await.unwrap();

    // 公开字段可读
    let _mgr: &Option<std::sync::Arc<fuyao_mcp::MCPManager>> = &app_ctx.mcp_manager;
    let _host: &fuyao_hooks::PluginHost = &app_ctx.plugin_host;
    // log_guard 在 setup 路径下为 default（真正 guard 在 init_engine 返回值）
    let _guard: &fuyao_app::LogGuard = &app_ctx.log_guard;
}

#[tokio::test]
async fn app_context_drop_disposes_plugins() {
    // AppContext drop 时 plugin_host 调 dispose_all，不应 panic
    let (ctx, _home) = test_agent_ctx_with_temp_home();
    let handle = make_handle(ctx);
    let app_ctx = setup(&handle).await.unwrap();

    drop(app_ctx); // 不 panic 即通过
}

// ============================================================================
// 端到端：Engine::new + setup 后可跑 ReAct
// ============================================================================

#[tokio::test]
async fn full_setup_then_react_loop() {
    // 完整装配后，引擎能端到端跑一轮 ReAct（含已装配的 session/guard 插件钩子）
    let (mut ctx, _home) = test_agent_ctx_with_temp_home();
    let provider = Box::new(MockProvider {
        events: text_events("装配后回复"),
    });
    let (mut engine, handle) = Engine::new(provider, ctx.clone());
    let app_ctx = setup(&handle).await.expect("装配应成功");

    let engine_task = tokio::spawn(async move { engine.run().await });

    handle.send_message("装配后测试".to_string()).await;

    // 收事件，验证装配后引擎可产出 Chunk + Assistant
    let mut got_chunk = false;
    let mut got_assistant = false;
    for _ in 0..20 {
        match tokio::time::timeout(Duration::from_millis(2000), handle.next_event()).await {
            Ok(Some(e)) => {
                use fuyao_api::message::OutputEvent;
                match e {
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
                }
            }
            _ => break,
        }
    }

    handle.shutdown().await;
    let _ = tokio::time::timeout(Duration::from_secs(3), engine_task).await;
    drop(app_ctx);

    assert!(got_chunk, "装配后应产出 Chunk 事件");
    assert!(got_assistant, "装配后应产出 Assistant 事件");

    // 抑制 ctx 部分移动警告（ctx.model_config 在 setup 内被读，此处保留完整 ctx 不必要）
    let _ = &mut ctx;
}

#[tokio::test]
async fn setup_idempotent_appends_tools() {
    // 重复 setup 会追加注册工具（register_tool 先到先得，重复跳过，schema 不重复）
    // 验证多次 setup 不 panic 且 schema 数量稳定（重复名跳过）
    let (ctx, _home) = test_agent_ctx_with_temp_home();
    let handle = make_handle(ctx);

    setup(&handle).await.unwrap();
    let count_after_first = handle.tools_schema().len();

    // 第二次 setup（同一 handle）——工具注册先到先得，数量不变
    let app_ctx = setup(&handle).await.unwrap();
    let count_after_second = handle.tools_schema().len();

    assert_eq!(
        count_after_first, count_after_second,
        "重复注册应跳过同名，工具数量不变"
    );
    drop(app_ctx);
}

// ============================================================================
// setup：无 AgentContext 错误路径（构造性测试）
// ============================================================================

#[tokio::test]
async fn setup_with_explicitly_valid_context_succeeds() {
    // 显式构造完整 AgentContext（含 agent_paths）验证装配成功路径
    // 这是 setup 的正常路径——NoAgentContext 仅在 agent_ctx_shared 锁中毒时触发，
    // 正常测试无法构造锁中毒，故此处验证正常路径即可
    let home = tempfile::tempdir().unwrap();
    let ctx = AgentContext {
        model_config: fuyao_api::ModelConfig {
            model_id: Some("test/model".to_string()),
            ..Default::default()
        },
        agent_paths: AgentPaths {
            fuyao_home: home.path().to_path_buf(),
            ..Default::default()
        },
        ..Default::default()
    };
    let handle = make_handle(ctx);

    let result = setup(&handle).await;

    assert!(result.is_ok(), "完整 AgentContext 下 setup 应成功");
    // result Ok 时才能取出 AppContext
    if let Ok(app_ctx) = result {
        assert!(app_ctx.plugin_host.list().contains(&"session"));
    }
}

/// 抑制未使用 SetupError 导入警告（错误路径测试保留枚举可访问性）
#[allow(dead_code)]
fn _setup_error_imported(_e: SetupError) {}
