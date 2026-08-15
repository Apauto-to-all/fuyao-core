//! 工具执行编排的单元测试
//!
//! 测逻辑分支（调度路径选择、通知顺序），不测运行时调度（无 sleep/计时）。
//! 测试 handler 全部为瞬时 echo / 瞬时 panic，毫秒级完成。
//!
//! 注意：本模块不 emit 事件（emit 由调用方 turn.rs 经 emit_to_history 统一处理）。
//! 测试只验证 `result_tx` 收到的 ToolExecResult。

use super::*;
use crate::tool_registry::ToolRegistryBuilder;
use futures_util::FutureExt;
use fuyao_api::{AgentPaths, CancellationToken, ToolDefinition, ToolEntry, ToolFn};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use tokio::sync::mpsc;

/// 测试用 AgentPaths：默认值即可（工具执行逻辑不依赖具体路径内容）
fn test_paths() -> AgentPaths {
    AgentPaths::default()
}

/// 构造测试用 ToolExecCtx（能力聚合内仅 event_forwarder 取 emitter 派生，其余为 None）
fn test_ctx(tools: Arc<ToolRegistry>) -> ToolExecCtx {
    let emitter = test_emitter();
    let forwarder = emitter.tx_clone();
    ToolExecCtx {
        tools,
        agent_paths: test_paths(),
        emitter,
        cancel: CancellationToken::new(),
        capabilities: ToolCapabilities {
            subagent_ops: None,
            event_forwarder: Some(forwarder),
            todo_store: None,
        },
    }
}

/// 构造一个 handler 恒返回固定串的工具条目
fn fixed_result_tool(name: &str, result: &str) -> ToolEntry {
    let result = result.to_string();
    let handler: ToolFn = Arc::new(move |_args, _ctx, _cancel| {
        let result = result.clone();
        Box::pin(async move { fuyao_api::ToolOutput::text(result) })
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
        Box::pin(async move { fuyao_api::ToolOutput::text(format!("result_{n}")) })
    });
    ToolEntry {
        definition: ToolDefinition::new(name, "测试工具"),
        handler,
        child_invisible: false,
    }
}

/// 构造一个首次 poll 即 panic 的工具条目（验证 panic 防护降级）
fn panic_tool(name: &str) -> ToolEntry {
    let handler: ToolFn = Arc::new(|_args, _ctx, _cancel| Box::pin(async { panic!("boom") }));
    ToolEntry {
        definition: ToolDefinition::new(name, "测试工具"),
        handler,
        child_invisible: false,
    }
}

/// 静音默认 panic hook（被测代码会捕获 panic，但默认 hook 仍向 stderr 打噪音）
///
/// 返回恢复函数：调用方在 panic 触发点结束后必须调用，避免影响其他测试的 panic 输出。
fn silence_panic_hook() -> impl FnOnce() {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    move || {
        std::panic::set_hook(prev_hook);
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
    let ctx = test_ctx(tools);
    let tc = make_tool_call("1", "unknown_tool", "{}");
    let result = execute_single(&tc, &ctx).await;
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
    let ctx = test_ctx(tools);
    let tc = make_tool_call("1", "test_tool", r#"{"key":"value"}"#);
    let result = execute_single(&tc, &ctx).await;
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
    let ctx = test_ctx(tools);
    let calls = vec![make_tool_call("1", "read", r#"{"name":"a"}"#)];
    let (tx, mut rx) = mpsc::channel(8);

    execute_tools(&calls, &tx, &ctx).await;
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
    let ctx = test_ctx(tools);
    let calls = vec![
        make_tool_call("1", "bash", r#"{"name":"a"}"#),
        make_tool_call("2", "bash", r#"{"name":"b"}"#),
    ];
    let (tx, mut rx) = mpsc::channel(8);

    execute_tools(&calls, &tx, &ctx).await;
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
    let ctx = test_ctx(tools);
    let calls = vec![
        make_tool_call("1", "glob", r#"{"name":"a"}"#),
        make_tool_call("2", "grep", r#"{"name":"b"}"#),
        make_tool_call("3", "glob", r#"{"name":"c"}"#),
    ];
    let (tx, mut rx) = mpsc::channel(16);

    execute_tools(&calls, &tx, &ctx).await;
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
    let ctx = test_ctx(tools);
    let calls: Vec<_> = (0..10)
        .map(|i| make_tool_call(&i.to_string(), "glob", &format!(r#"{{"name":"{i}"}}"#)))
        .collect();
    let (tx, mut rx) = mpsc::channel(32);

    let config = fuyao_api::ToolRunnerConfig {
        max_concurrent: 1,
        ..Default::default()
    };
    execute_parallel(&calls, &tx, &ctx, &config).await;
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
    let ctx = test_ctx(tools);
    let calls = vec![
        make_tool_call("1", "glob", r#"{"name":"a"}"#),
        make_tool_call("2", "glob", r#"{"name":"b"}"#),
        make_tool_call("3", "glob", r#"{"name":"c"}"#),
    ];
    let expected = calls.len();
    let (tx, mut rx) = mpsc::channel(expected);

    execute_tools(&calls, &tx, &ctx).await;
    let results = collect_results(&tx, &mut rx, expected).await;

    assert_eq!(results.len(), expected, "通知次数应等于工具调用数");
}

// ---- panic 防护（handler 内部 panic 降级为错误输出，不沿 session task 传播） ----

#[tokio::test]
async fn execute_single_handler_panic_degrades_to_error_output() {
    let restore_hook = silence_panic_hook();

    let tools = Arc::new(
        ToolRegistryBuilder::default()
            .register(panic_tool("boom_tool"))
            .build(),
    );
    let ctx = test_ctx(tools);
    let tc = make_tool_call("1", "boom_tool", "{}");

    // 外层再包一层 catch_unwind：若降级逻辑回归（panic 向外传播），测试以明确断言消息失败，
    // 而不是让 panic 炸掉测试本身；两种结局都先恢复 hook
    let outcome = AssertUnwindSafe(execute_single(&tc, &ctx))
        .catch_unwind()
        .await;
    restore_hook();

    let result = match outcome {
        Ok(result) => result,
        Err(_) => panic!("handler panic 应被降级为错误输出，而不是向外传播"),
    };

    assert_eq!(result.tool_name, "boom_tool");
    assert_eq!(result.tool_call_id, "1");
    assert!(
        result.content.contains("内部错误"),
        "应含「内部错误」提示，实际：{}",
        result.content
    );
    assert!(
        result.content.contains("panic") && result.content.contains("boom"),
        "应透传 panic 消息帮助定位，实际：{}",
        result.content
    );
}

#[tokio::test]
async fn sequential_batch_panic_notifies_error_and_continues() {
    // bash 在 never_parallel 默认列表 → 整批串行；
    // 首个工具 panic 降级为错误输出并正常通知，后续工具继续执行
    let restore_hook = silence_panic_hook();

    let tools = Arc::new(
        ToolRegistryBuilder::default()
            .register(panic_tool("bash"))
            .register(echo_name_tool("read"))
            .build(),
    );
    let ctx = test_ctx(tools);
    let calls = vec![
        make_tool_call("1", "bash", "{}"),
        make_tool_call("2", "read", r#"{"name":"a"}"#),
    ];
    let (tx, mut rx) = mpsc::channel(8);

    let outcome = AssertUnwindSafe(execute_tools(&calls, &tx, &ctx))
        .catch_unwind()
        .await;
    restore_hook();

    match outcome {
        Ok(()) => {}
        Err(_) => panic!("串行批次中单个工具 panic 应降级为错误输出，而不是炸掉整批"),
    }

    let results = collect_results(&tx, &mut rx, calls.len()).await;
    assert_eq!(results.len(), 2, "panic 工具与正常工具都应产出结果");
    assert_eq!(results[0].tool_name, "bash");
    assert!(
        results[0].content.contains("内部错误"),
        "panic 应降级为错误输出，实际：{}",
        results[0].content
    );
    assert_eq!(results[1].tool_name, "read");
    assert_eq!(
        results[1].content, "result_a",
        "panic 后的后续工具应正常执行"
    );
}

#[tokio::test]
async fn parallel_batch_panic_degrades_and_siblings_complete() {
    // glob/grep 在 parallel_safe 默认列表 → 并行；
    // panic 工具在 spawn task 内降级为错误输出，兄弟任务不受连坐
    let restore_hook = silence_panic_hook();

    let tools = Arc::new(
        ToolRegistryBuilder::default()
            .register(panic_tool("grep"))
            .register(echo_name_tool("glob"))
            .build(),
    );
    let ctx = test_ctx(tools);
    let calls = vec![
        make_tool_call("1", "grep", "{}"),
        make_tool_call("2", "glob", r#"{"name":"a"}"#),
    ];
    let (tx, mut rx) = mpsc::channel(8);

    let outcome = AssertUnwindSafe(execute_tools(&calls, &tx, &ctx))
        .catch_unwind()
        .await;
    restore_hook();

    match outcome {
        Ok(()) => {}
        Err(_) => panic!("并行批次中单个工具 panic 应降级为错误输出，而不是炸掉整批"),
    }

    let results = collect_results(&tx, &mut rx, calls.len()).await;
    assert_eq!(results.len(), 2, "panic 工具与正常工具都应产出结果");
    // 完成顺序不确定，按工具名匹配断言
    let degraded = results
        .iter()
        .find(|r| r.tool_name == "grep")
        .expect("panic 工具应有降级结果");
    assert!(
        degraded.content.contains("内部错误"),
        "panic 应降级为错误输出，实际：{}",
        degraded.content
    );
    let sibling = results
        .iter()
        .find(|r| r.tool_name == "glob")
        .expect("兄弟工具应正常完成");
    assert_eq!(sibling.content, "result_a", "panic 不应连坐兄弟任务");
}
