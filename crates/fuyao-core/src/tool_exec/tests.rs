//! 工具执行编排的单元测试
//!
//! 测逻辑分支（调度路径选择、通知顺序），不测运行时调度（无 sleep/计时）。
//! 测试 handler 全部为瞬时 echo，毫秒级完成。
//!
//! 注意：本模块不 emit 事件（emit 由调用方 turn.rs 经 emit_to_history 统一处理）。
//! 测试只验证 `result_tx` 收到的 ToolExecResult。

use super::*;
use crate::tool_registry::{ToolEntry, ToolRegistryBuilder};
use fuyao_api::{AgentPaths, CancellationToken, ToolDefinition, ToolFn};
use std::sync::Arc;
use tokio::sync::mpsc;

/// 测试用 AgentPaths：默认值即可（工具执行逻辑不依赖具体路径内容）
fn test_paths() -> AgentPaths {
    AgentPaths::default()
}

/// 构造一个 handler 恒返回固定串的工具条目
fn fixed_result_tool(name: &str, result: &str) -> ToolEntry {
    let result = result.to_string();
    let handler: ToolFn = Arc::new(move |_args, _ctx, _cancel| {
        let result = result.clone();
        Box::pin(async move { result })
    });
    ToolEntry {
        definition: ToolDefinition::new(name, "测试工具"),
        handler,
        child_invisible: false,
    }
}

/// 构造一个按 args.name 返回 result_{name} 的工具条目
fn echo_name_tool(name: &str) -> ToolEntry {
    let handler: ToolFn = Arc::new(|args, _ctx, _cancel| {
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
        child_invisible: false,
    }
}

fn make_tool_call(id: &str, name: &str, args: &str) -> ToolCallData {
    ToolCallData {
        id: id.to_string(),
        name: name.to_string(),
        arguments: args.to_string(),
    }
}

/// 构造 Emitter（不需要验证事件，只用于 session_id 日志）
fn test_emitter() -> Emitter {
    let (tx, _rx) = mpsc::unbounded_channel::<fuyao_api::message::OutputEvent>();
    Emitter::new(tx, "test-session".to_string())
}

/// 收集所有 result（容量足够大，保证不阻塞）
async fn collect_results(
    tx: &Sender<ToolExecResult>,
    rx: &mut mpsc::Receiver<ToolExecResult>,
    expected: usize,
) -> Vec<ToolExecResult> {
    let mut results = Vec::with_capacity(expected);
    while results.len() < expected {
        if let Some(r) = rx.recv().await {
            results.push(r);
        } else {
            break;
        }
    }
    let _ = tx; // 引用 tx 防止提前 drop
    results
}

// ---- execute_single ----

#[tokio::test]
async fn execute_single_unknown_tool() {
    let tools = Arc::new(ToolRegistryBuilder::default().build());
    let tc = make_tool_call("1", "unknown_tool", "{}");
    let result = execute_single(
        &tc,
        &tools,
        &test_paths(),
        "s1",
        &CancellationToken::new(),
        None,
        None,
    )
    .await;
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
    let result = execute_single(
        &tc,
        &tools,
        &test_paths(),
        "s1",
        &CancellationToken::new(),
        None,
        None,
    )
    .await;
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
    let emitter = test_emitter();
    let calls = vec![make_tool_call("1", "read", r#"{"name":"a"}"#)];
    let (tx, mut rx) = mpsc::channel(8);

    execute_tools(
        &calls,
        &tools,
        &test_paths(),
        &emitter,
        &tx,
        &CancellationToken::new(),
        None,
    )
    .await;
    let results = collect_results(&tx, &mut rx, calls.len()).await;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content, "result_a");
}

#[tokio::test]
async fn never_parallel_tool_goes_sequential() {
    // bash 在 never_parallel 默认列表中 → 串行（即便有多个）
    let tools = Arc::new(
        ToolRegistryBuilder::default()
            .register(echo_name_tool("bash"))
            .build(),
    );
    let emitter = test_emitter();
    let calls = vec![
        make_tool_call("1", "bash", r#"{"name":"a"}"#),
        make_tool_call("2", "bash", r#"{"name":"b"}"#),
    ];
    let (tx, mut rx) = mpsc::channel(8);

    execute_tools(
        &calls,
        &tools,
        &test_paths(),
        &emitter,
        &tx,
        &CancellationToken::new(),
        None,
    )
    .await;
    let results = collect_results(&tx, &mut rx, calls.len()).await;

    // 串行：结果按提交序
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].content, "result_a");
    assert_eq!(results[1].content, "result_b");
}

// ---- 并行路径 ----

#[tokio::test]
async fn parallel_executes_all() {
    // glob/grep 在 parallel_safe 默认列表中 → 并行
    let tools = Arc::new(
        ToolRegistryBuilder::default()
            .register(echo_name_tool("glob"))
            .register(echo_name_tool("grep"))
            .build(),
    );
    let emitter = test_emitter();
    let calls = vec![
        make_tool_call("1", "glob", r#"{"name":"a"}"#),
        make_tool_call("2", "grep", r#"{"name":"b"}"#),
        make_tool_call("3", "glob", r#"{"name":"c"}"#),
    ];
    let (tx, mut rx) = mpsc::channel(16);

    execute_tools(
        &calls,
        &tools,
        &test_paths(),
        &emitter,
        &tx,
        &CancellationToken::new(),
        None,
    )
    .await;
    let results = collect_results(&tx, &mut rx, calls.len()).await;

    assert_eq!(results.len(), 3);
    // 收集所有 result 的 content（顺序可能是完成序，用 sort 验证集合一致）
    let mut contents: Vec<_> = results.iter().map(|r| r.content.clone()).collect();
    contents.sort();
    assert_eq!(contents, vec!["result_a", "result_b", "result_c"]);
}

#[tokio::test]
async fn parallel_respects_max_concurrent() {
    // max_concurrent=1 时，并行路径退化为「一次一个」，但结果仍全跑完
    let tools = Arc::new(
        ToolRegistryBuilder::default()
            .register(echo_name_tool("glob"))
            .build(),
    );
    let emitter = test_emitter();
    let calls: Vec<_> = (0..10)
        .map(|i| make_tool_call(&i.to_string(), "glob", &format!(r#"{{"name":"{i}"}}"#)))
        .collect();
    let (tx, mut rx) = mpsc::channel(32);

    let config = fuyao_api::ToolRunnerConfig {
        max_concurrent: 1,
        ..Default::default()
    };
    execute_parallel(
        &calls,
        &tools,
        &test_paths(),
        &emitter,
        &tx,
        &config,
        &CancellationToken::new(),
        None,
        None,
    )
    .await;
    let results = collect_results(&tx, &mut rx, calls.len()).await;

    assert_eq!(results.len(), 10);
}

#[tokio::test]
async fn parallel_notify_count_matches_calls() {
    // 并行：通知次数 = 工具调用数（完成一个通知一个）
    let tools = Arc::new(
        ToolRegistryBuilder::default()
            .register(echo_name_tool("glob"))
            .build(),
    );
    let emitter = test_emitter();
    let calls = vec![
        make_tool_call("1", "glob", r#"{"name":"a"}"#),
        make_tool_call("2", "glob", r#"{"name":"b"}"#),
        make_tool_call("3", "glob", r#"{"name":"c"}"#),
    ];
    let expected = calls.len();
    let (tx, mut rx) = mpsc::channel(expected);

    execute_tools(
        &calls,
        &tools,
        &test_paths(),
        &emitter,
        &tx,
        &CancellationToken::new(),
        None,
    )
    .await;
    let results = collect_results(&tx, &mut rx, expected).await;

    assert_eq!(results.len(), expected, "通知次数应等于工具调用数");
}
