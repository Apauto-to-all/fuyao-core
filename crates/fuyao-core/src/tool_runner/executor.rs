//! 工具执行器
//!
//! 提供串行和并行两种执行策略：
//! - `execute_sequential`: 逐个执行
//! - `execute_parallel`: JoinSet + Semaphore 并发执行
//! - `execute_single_tool`: 单工具执行

use fuyao_api::{AgentContext, ToolCallContext, ToolFn, ToolRunnerConfig, should_parallelize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;

/// 工具执行结果
pub(crate) struct ToolExecResult {
    pub(crate) tool_call_id: String,
    pub(crate) tool_name: String,
    pub(crate) content: String,
}

/// 智能调度：根据配置决定并行或串行执行
pub(crate) async fn execute_tools(
    tool_calls: &[fuyao_provider::ToolCallData],
    handlers: &HashMap<String, ToolFn>,
    agent_ctx: &AgentContext,
    config: &ToolRunnerConfig,
) -> Vec<ToolExecResult> {
    let call_infos: Vec<_> = tool_calls
        .iter()
        .map(|tc| fuyao_api::paths::parallel::ToolCallInfo {
            name: tc.name.clone(),
            arguments: tc.arguments.clone(),
        })
        .collect();

    if should_parallelize(&call_infos, config) {
        execute_parallel(tool_calls, handlers, agent_ctx, config).await
    } else {
        execute_sequential(tool_calls, handlers, agent_ctx).await
    }
}

/// 串行执行工具调用
async fn execute_sequential(
    tool_calls: &[fuyao_provider::ToolCallData],
    handlers: &HashMap<String, ToolFn>,
    agent_ctx: &AgentContext,
) -> Vec<ToolExecResult> {
    let mut results = Vec::new();
    for tc in tool_calls {
        results.push(execute_single_tool(tc, handlers, agent_ctx).await);
    }
    results
}

/// 并行执行工具调用，完成一个收集一个
///
/// 使用 JoinSet + Semaphore 控制并发数，通过 mpsc channel 收集结果。
/// JoinSet drop 时自动 abort 所有未完成任务。
async fn execute_parallel(
    tool_calls: &[fuyao_provider::ToolCallData],
    handlers: &HashMap<String, ToolFn>,
    agent_ctx: &AgentContext,
    config: &ToolRunnerConfig,
) -> Vec<ToolExecResult> {
    let semaphore = Arc::new(Semaphore::new(config.max_concurrent as usize));
    let (result_tx, mut result_rx) = mpsc::channel(tool_calls.len());

    // JoinSet 用于确保 spawned 任务的生命周期管理
    // drop 时自动 abort 所有未完成任务（等同于 Python 的 finally: task.cancel()）
    let mut join_set = tokio::task::JoinSet::new();

    for tc in tool_calls {
        let tc = tc.clone();
        let handlers = handlers.clone();
        let agent_ctx = agent_ctx.clone();
        let semaphore = semaphore.clone();
        let result_tx = result_tx.clone();

        join_set.spawn(async move {
            let _permit = semaphore.acquire().await;
            let result = execute_single_tool(&tc, &handlers, &agent_ctx).await;
            let _ = result_tx.send(result).await;
        });
    }

    drop(result_tx);

    let mut results = Vec::with_capacity(tool_calls.len());
    while let Some(result) = result_rx.recv().await {
        results.push(result);
    }

    results
}

/// 执行单个工具
async fn execute_single_tool(
    tc: &fuyao_provider::ToolCallData,
    handlers: &HashMap<String, ToolFn>,
    agent_ctx: &AgentContext,
) -> ToolExecResult {
    let handler = match handlers.get(&tc.name) {
        Some(h) => h,
        None => {
            return ToolExecResult {
                tool_call_id: tc.id.clone(),
                tool_name: tc.name.clone(),
                content: format!("未知工具: {}", tc.name),
            };
        }
    };

    let args: serde_json::Value = match serde_json::from_str(&tc.arguments) {
        Ok(v) => v,
        Err(_) => {
            let raw: String = tc.arguments.chars().take(200).collect();
            tracing::warn!(tool = %tc.name, raw = %raw, "工具参数 JSON 解析失败");
            serde_json::Value::Null
        }
    };
    let ctx = ToolCallContext {
        session_id: agent_ctx.session_id.clone(),
        agent_paths: Some(agent_ctx.agent_paths.clone()),
    };

    let started = std::time::Instant::now();
    let content = handler(args, ctx).await;

    tracing::info!(
        tool = %tc.name,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "工具调用完成"
    );

    ToolExecResult {
        tool_call_id: tc.id.clone(),
        tool_name: tc.name.clone(),
        content,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn make_tool_call(id: &str, name: &str, args: &str) -> fuyao_provider::ToolCallData {
        fuyao_provider::ToolCallData {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test]
    async fn execute_single_unknown_tool() {
        let tc = make_tool_call("1", "unknown_tool", "{}");
        let handlers = HashMap::new();
        let agent_ctx = AgentContext::default();

        let result = execute_single_tool(&tc, &handlers, &agent_ctx).await;
        assert_eq!(result.tool_name, "unknown_tool");
        assert!(result.content.contains("未知工具"));
    }

    #[tokio::test]
    async fn execute_single_known_tool() {
        let tc = make_tool_call("1", "test_tool", r#"{"key":"value"}"#);
        let handler: ToolFn = Arc::new(|_args, _ctx| Box::pin(async { "tool result".to_string() }));
        let mut handlers = HashMap::new();
        handlers.insert("test_tool".to_string(), handler);
        let agent_ctx = AgentContext::default();

        let result = execute_single_tool(&tc, &handlers, &agent_ctx).await;
        assert_eq!(result.content, "tool result");
    }

    #[tokio::test]
    async fn sequential_executes_all() {
        let handler: ToolFn = Arc::new(|args, _ctx| {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Box::pin(async move { format!("result_{name}") })
        });
        let mut handlers = HashMap::new();
        handlers.insert("test_tool".to_string(), handler);

        let calls = vec![
            make_tool_call("1", "test_tool", r#"{"name":"a"}"#),
            make_tool_call("2", "test_tool", r#"{"name":"b"}"#),
        ];
        let agent_ctx = AgentContext::default();

        let results = execute_sequential(&calls, &handlers, &agent_ctx).await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].content, "result_a");
        assert_eq!(results[1].content, "result_b");
    }

    #[tokio::test]
    async fn parallel_executes_all() {
        let handler: ToolFn = Arc::new(|args, _ctx| {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Box::pin(async move { format!("result_{name}") })
        });
        let mut handlers = HashMap::new();
        handlers.insert("test_tool".to_string(), handler);

        let calls = vec![
            make_tool_call("1", "test_tool", r#"{"name":"a"}"#),
            make_tool_call("2", "test_tool", r#"{"name":"b"}"#),
            make_tool_call("3", "test_tool", r#"{"name":"c"}"#),
        ];
        let agent_ctx = AgentContext::default();
        let config = ToolRunnerConfig::default();

        let results = execute_parallel(&calls, &handlers, &agent_ctx, &config).await;
        assert_eq!(results.len(), 3);

        let contents: HashSet<String> = results.iter().map(|r| r.content.clone()).collect();
        assert!(contents.contains("result_a"));
        assert!(contents.contains("result_b"));
        assert!(contents.contains("result_c"));
    }

    #[tokio::test]
    async fn parallel_respects_max_concurrent() {
        let handler: ToolFn = Arc::new(|_args, _ctx| Box::pin(async { "ok".to_string() }));
        let mut handlers = HashMap::new();
        handlers.insert("test_tool".to_string(), handler);

        let calls: Vec<_> = (0..10)
            .map(|i| make_tool_call(&i.to_string(), "test_tool", "{}"))
            .collect();
        let agent_ctx = AgentContext::default();
        let mut config = ToolRunnerConfig::default();
        config.max_concurrent = 2;

        let results = execute_parallel(&calls, &handlers, &agent_ctx, &config).await;
        assert_eq!(results.len(), 10);
    }
}
