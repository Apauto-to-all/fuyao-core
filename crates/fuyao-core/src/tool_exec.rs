//! 工具执行编排（session 级）
//!
//! 工具注册是引擎级共享（[`crate::tool_registry::ToolRegistry`]），
//! 工具执行是 session 级——各 task 在自己的 ReAct 循环里查 handler 执行，互不等。
//!
//! 工具结果不走队列：它是 ReAct 循环的内部中间产物，产生时 task 正握着控制权，
//! 直接 push 进 task 本地的 `session.messages`，然后 continue 回循环顶部。
//!
//! 关键设计（避开归档结构债）：
//! - **完成一个 emit 一个**：每执行完一个工具就立即发 ToolResult 事件 + 返回结果，
//!   不等所有工具都跑完才批量发出（归档是收齐再 emit，中断时丢已完成结果）。
//! - **串行执行**：第三步先串行跑通循环，并行编排依赖外围 `ToolRunnerConfig`，
//!   留 TODO。
//! - **容错降级**：未知工具不报错（返回提示字符串），参数解析失败用 `Value::Null`。
//! - **工具的并发安全是工具自己的责任**：引擎只负责"让多个 session 能同时调工具"，
//!   不介入排序/加锁。

use crate::emit::Emitter;
use crate::tool_registry::ToolRegistry;
use fuyao_api::ToolCallContext;
use fuyao_api::message::output::{ToolResultMessage, ToolResultPayload};
use fuyao_api::message::{EventBase, OutputEvent};
use fuyao_provider::ToolCallData;

/// 单个工具执行的结果
pub(crate) struct ToolExecResult {
    /// 对应的 LLM 工具调用 ID（回填进 `Message::tool_result`）
    pub tool_call_id: String,
    /// 工具名
    pub tool_name: String,
    /// 工具返回的内容字符串（handler 返回的 `String`，非结构化）
    pub content: String,
}

/// 串行执行一批工具调用
///
/// 遍历 `tool_calls`，逐个查注册表 → 调 handler → 立即 emit ToolResult 事件 → 收集结果。
/// 完成一个 emit 一个，不等全部跑完。返回结果向量供调用方 push 进 messages。
///
/// `agent_paths` 注入 `ToolCallContext` 供工具访问路径；`emitter` 负责事件标签。
pub(crate) async fn execute_tools(
    tool_calls: &[ToolCallData],
    registry: &ToolRegistry,
    agent_paths: &fuyao_api::AgentPaths,
    emitter: &Emitter,
) -> Vec<ToolExecResult> {
    let session_id = emitter.session_id().to_string();
    let mut results = Vec::with_capacity(tool_calls.len());

    for tc in tool_calls {
        let result = execute_single(tc, registry, agent_paths, &session_id).await;

        // 完成一个 emit 一个：立即发 ToolResult 事件
        emitter
            .emit(OutputEvent::ToolResult(ToolResultMessage {
                base: EventBase::default(),
                payload: ToolResultPayload {
                    tool_call_id: result.tool_call_id.clone(),
                    tool_name: result.tool_name.clone(),
                    content: result.content.clone(),
                },
            }))
            .await;

        results.push(result);
    }

    results
}

/// 执行单个工具调用
///
/// 查注册表拿 handler → 解析参数 → 构建 `ToolCallContext` → 调 handler。
/// 容错：未知工具返回提示字符串；参数解析失败用 `Value::Null`。
async fn execute_single(
    tc: &ToolCallData,
    registry: &ToolRegistry,
    agent_paths: &fuyao_api::AgentPaths,
    session_id: &str,
) -> ToolExecResult {
    let tool_name = tc.name.clone();
    let tool_call_id = tc.id.clone();

    let Some(entry) = registry.get(&tool_name) else {
        // 未知工具：容错降级，返回明确提示而非报错
        tracing::warn!(tool_name = %tool_name, "未知工具");
        return ToolExecResult {
            tool_call_id,
            content: format!("未知工具: {tool_name}"),
            tool_name,
        };
    };

    // 解析参数 JSON：失败用 Value::Null，不报错
    let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_else(|e| {
        tracing::warn!(tool_name = %tool_name, cause = %e, "工具参数解析失败，使用 null");
        serde_json::Value::Null
    });

    // 构建上下文：注入 session_id + agent_paths（工具据此访问路径、识别会话）
    let ctx = ToolCallContext {
        session_id: Some(session_id.to_string()),
        agent_paths: Some(agent_paths.clone()),
    };

    let started = std::time::Instant::now();
    let content = (entry.handler)(args, ctx).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    tracing::info!(
        tool_name = %tool_name,
        elapsed_ms,
        "工具执行完成"
    );

    ToolExecResult {
        tool_call_id,
        tool_name,
        content,
    }
}
