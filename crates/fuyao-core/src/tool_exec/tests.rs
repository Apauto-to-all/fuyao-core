//! 工具执行编排的单元测试
//!
//! 测逻辑分支（调度路径选择、结果顺序、事件数），不测运行时调度（无 sleep/计时）。
//! 测试 handler 全部为瞬时 echo，毫秒级完成。

use super::*;
use crate::tool_registry::{ToolEntry, ToolRegistryBuilder};
use fuyao_api::{AgentPaths, ToolDefinition, ToolFn};
use std::sync::Arc;
use tokio::sync::mpsc;

/// 测试用 AgentPaths：默认值即可（工具执行逻辑不依赖具体路径内容）
fn test_paths() -> AgentPaths {
    AgentPaths::default()
}

/// 构造空 SharedHooks（无拦截/观察钩子，管道纯透传）
fn empty_hooks() -> fuyao_hooks::SharedHooks {
    Arc::new(tokio::sync::Mutex::new(
        fuyao_hooks::HooksRegistry::default(),
    ))
}

/// 构造一个 handler 恒返回固定串的工具条目
fn fixed_result_tool(name: &str, result: &str) -> ToolEntry {
    let result = result.to_string();
    let handler: ToolFn = Arc::new(move |_args, _ctx| {
        let result = result.clone();
        Box::pin(async move { result })
    });
    ToolEntry {
        definition: ToolDefinition::new(name, "测试工具"),
        handler,
    }
}

/// 构造一个按 args.name 返回 result_{name} 的工具条目
fn echo_name_tool(name: &str) -> ToolEntry {
    let handler: ToolFn = Arc::new(|args, _ctx| {
        let n = args
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Box::pin(async move { format!("result_{n}") })
    });
    ToolEntry {
        definition: ToolDefinition::new(name, "测试工具"),
        handler,
    }
}

fn make_tool_call(id: &str, name: &str, args: &str) -> ToolCallData {
    ToolCallData {
        id: id.to_string(),
        name: name.to_string(),
        arguments: args.to_string(),
    }
}

/// 构造 Emitter + 接收端，用于验证 emit 出的事件
fn test_emitter(buf: usize) -> (Emitter, mpsc::Receiver<OutputEvent>) {
    let (tx, rx) = mpsc::channel(buf);
    (Emitter::new(tx, "test-session".to_string()), rx)
}

// ---- execute_single ----

#[tokio::test]
async fn execute_single_unknown_tool() {
    let tools = Arc::new(ToolRegistryBuilder::default().build());
    let tc = make_tool_call("1", "unknown_tool", "{}");
    let result = execute_single(&tc, &tools, &test_paths(), "s1").await;
    assert_eq!(result.tool_name, "unknown_tool");
    assert!(result.content.contains("未知工具"));
}

#[tokio::test]
async fn execute_single_known_tool() {
    let tools = Arc::new(
        ToolRegistryBuilder::default()
            .register(fixed_result_tool("test_tool", "tool result"))
            .build(),
    );
    let tc = make_tool_call("1", "test_tool", r#"{"key":"value"}"#);
    let result = execute_single(&tc, &tools, &test_paths(), "s1").await;
    assert_eq!(result.content, "tool result");
}

// ---- execute_tools 调度（串行路径） ----

#[tokio::test]
async fn single_call_goes_sequential() {
    // 单个调用：should_parallelize 返回 false，走串行
    let tools = Arc::new(
        ToolRegistryBuilder::default()
            .register(echo_name_tool("read"))
            .build(),
    );
    let (emitter, mut rx) = test_emitter(8);
    let calls = vec![make_tool_call("1", "read", r#"{"name":"a"}"#)];

    let results = execute_tools(&calls, &tools, &test_paths(), &emitter, &empty_hooks()).await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content, "result_a");

    // 串行完成一个 emit 一个
    let ev = rx.recv().await.expect("应有 ToolResult 事件");
    match ev {
        OutputEvent::ToolResult(m) => assert_eq!(m.payload.content, "result_a"),
        other => panic!("期望 ToolResult，实际 {other:?}"),
    }
}

#[tokio::test]
async fn never_parallel_tool_goes_sequential() {
    // bash 在 never_parallel 默认列表中 → 串行（即便有多个）
    let tools = Arc::new(
        ToolRegistryBuilder::default()
            .register(echo_name_tool("bash"))
            .build(),
    );
    let (emitter, _rx) = test_emitter(8);
    let calls = vec![
        make_tool_call("1", "bash", r#"{"name":"a"}"#),
        make_tool_call("2", "bash", r#"{"name":"b"}"#),
    ];

    let results = execute_tools(&calls, &tools, &test_paths(), &emitter, &empty_hooks()).await;
    // 串行：结果按提交序
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].content, "result_a");
    assert_eq!(results[1].content, "result_b");
}

// ---- 并行路径 ----

#[tokio::test]
async fn parallel_executes_all_and_preserves_submit_order() {
    // glob/grep 在 parallel_safe 默认列表中 → 并行
    let tools = Arc::new(
        ToolRegistryBuilder::default()
            .register(echo_name_tool("glob"))
            .register(echo_name_tool("grep"))
            .build(),
    );
    let (emitter, _rx) = test_emitter(16);
    let calls = vec![
        make_tool_call("1", "glob", r#"{"name":"a"}"#),
        make_tool_call("2", "grep", r#"{"name":"b"}"#),
        make_tool_call("3", "glob", r#"{"name":"c"}"#),
    ];

    let results = execute_tools(&calls, &tools, &test_paths(), &emitter, &empty_hooks()).await;
    assert_eq!(results.len(), 3);
    // 返回顺序 = 提交顺序（即便并行执行完成顺序可能不同）
    assert_eq!(results[0].content, "result_a");
    assert_eq!(results[1].content, "result_b");
    assert_eq!(results[2].content, "result_c");
}

#[tokio::test]
async fn parallel_respects_max_concurrent() {
    // max_concurrent=1 时，并行路径退化为「一次一个」，但结果仍全跑完
    let tools = Arc::new(
        ToolRegistryBuilder::default()
            .register(echo_name_tool("glob"))
            .build(),
    );
    let (emitter, _rx) = test_emitter(32);
    let calls: Vec<_> = (0..10)
        .map(|i| make_tool_call(&i.to_string(), "glob", &format!(r#"{{"name":"{i}"}}"#)))
        .collect();

    // 临时覆盖配置：max_concurrent=1（通过临时 TOML set_config 不可行，改用直接测 execute_parallel）
    let config = fuyao_api::ToolRunnerConfig {
        max_concurrent: 1,
        ..Default::default()
    };
    let results = execute_parallel(
        &calls,
        &tools,
        &test_paths(),
        &emitter,
        &empty_hooks(),
        &config,
    )
    .await;
    assert_eq!(results.len(), 10);
    // 提交序保持
    for (i, r) in results.iter().enumerate() {
        assert_eq!(r.content, format!("result_{i}"));
    }
}

#[tokio::test]
async fn parallel_emit_count_matches_calls() {
    // 并行：emit 事件数 = 工具调用数（完成一个 emit 一个）
    // 注意：execute_tools 持有 emitter 引用 emit 完才返回，返回后 emitter 仍存活，
    // channel 不会关闭，故不能用「recv 返回 None 退出」模式——数够 calls.len() 个即停。
    let tools = Arc::new(
        ToolRegistryBuilder::default()
            .register(echo_name_tool("glob"))
            .build(),
    );
    let calls = vec![
        make_tool_call("1", "glob", r#"{"name":"a"}"#),
        make_tool_call("2", "glob", r#"{"name":"b"}"#),
        make_tool_call("3", "glob", r#"{"name":"c"}"#),
    ];
    let expected = calls.len();

    let (emitter, mut rx) = test_emitter(expected);
    let _ = execute_tools(&calls, &tools, &test_paths(), &emitter, &empty_hooks()).await;

    let mut count = 0;
    while count < expected {
        let ev = rx.recv().await.expect("事件数少于工具调用数");
        assert!(matches!(ev, OutputEvent::ToolResult(_)));
        count += 1;
    }
    assert_eq!(count, expected, "emit 事件数应等于工具调用数");
}
