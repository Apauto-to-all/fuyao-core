//! 工具执行编排（session 级）
//!
//! 工具注册是引擎级共享（[`crate::tool_registry::ToolRegistry`]），
//! 工具执行是 session 级——各 task 在自己的 ReAct 循环里查 handler 执行，互不等。
//!
//! 工具结果不走队列：它是 ReAct 循环的内部中间产物，产生时 task 正握着控制权，
//! 通过 `result_tx` 通知调用方，调用方立即走 `emit_to_history`（拦截 → push messages → 发事件）。
//!
//! 关键设计：
//! - **完成一个通知一个**：每执行完一个工具就立即通过 `result_tx` 发送结果，
//!   不等所有工具都跑完才批量发出（避开归档「收齐再 emit、中断丢已完成结果」的结构债）。
//!   并行版用 `JoinSet::join_next` 逐个收，完成即通知；串行版在循环里逐个通知。
//! - **本模块不再 emit 事件**：emit/拦截/push session.messages 是调用方（turn.rs）的职责，
//!   经 `emit_to_history` 统一入口完成。本模块只负责"执行 + 通知"。
//! - **智能调度**：`should_parallelize` 判定批次能否并行（never_parallel / 路径重叠 /
//!   parallel_safe），能并行走 `execute_parallel`（JoinSet + Semaphore），否则走 `execute_sequential`。
//! - **容错降级**：未知工具不报错（返回提示字符串），参数解析失败用 `Value::Null`。
//! - **工具的并发安全是工具自己的责任**：引擎只负责「让多个 session 能同时调同一个工具」，
//!   不介入排序/加锁。Semaphore/JoinSet 是每次调用的局部对象，session 间互不可见、互不协调。
//!
//! 关于两层并发的边界（与多 session 并发正交、不冲突）：
//! - **session 间并发**（已实现）：每 session 一个 tokio task，互不等。
//! - **session 内工具并行**（本模块）：单 task 内用 JoinSet 把一批 tool_calls 并发跑。
//!   两层叠加时，各 session 的 `execute_parallel` 各自创建局部 Semaphore/JoinSet，
//!   跨 session 零协调——这正是「引擎不为工具调用建全局队列、不排队、不协调」的字面落实。

mod parallel;

use crate::emit::Emitter;
use crate::tool_registry::ToolRegistry;
use fuyao_api::message::OutputEvent;
use fuyao_api::{CancellationToken, TodoStoreOps, ToolCallContext};
use fuyao_provider::ToolCallData;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{Sender, UnboundedSender};
use tokio::task::JoinSet;

/// 单个工具执行的结果
pub(crate) struct ToolExecResult {
    /// 对应的 LLM 工具调用 ID（回填进 `Message::tool_result`）
    pub tool_call_id: String,
    /// 工具名
    pub tool_name: String,
    /// 工具返回的内容字符串（handler 返回的 `String`，非结构化）
    pub content: String,
}

/// 执行一批工具调用（智能调度：能并行则并行，否则串行）
///
/// 空批次直接返回。否则读 `[tools.runner]` 配置，`should_parallelize` 判定走并行还是串行。
/// 两种路径都遵循「完成一个通知一个」——通过 `result_tx` 发送结果，调用方据此立即
/// 走 `emit_to_history`（拦截 → push session.messages → 发送事件 → 观察）。
///
/// 工具事件（ToolResult OutputEvent）的 emit 与拦截不在本模块做——归调用方统一处理，
/// 保证「拦截 → 存储 → 消费」三者数据一致。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_tools(
    tool_calls: &[ToolCallData],
    tools: &Arc<ToolRegistry>,
    agent_paths: &fuyao_api::AgentPaths,
    emitter: &Emitter,
    result_tx: &Sender<ToolExecResult>,
    cancel: &CancellationToken,
    subagent_ops: Option<std::sync::Weak<dyn fuyao_api::SubagentOps>>,
    todo_store: Option<Arc<dyn TodoStoreOps>>,
) {
    if tool_calls.is_empty() {
        return;
    }

    // 父 session 出站通道的直送克隆：供子代理类工具把子 session 的中间事件转发过来
    // （绕过 emitter.emit 的 stamp_session_id——子事件已自带 child session_id 标签）
    let event_forwarder = Some(emitter.tx_clone());

    // 工具并发策略从全局配置读取（`[tools.runner]`），运行期只读
    let config = fuyao_api::get_config().tools.runner.clone();

    // 映射为轻量 ToolCallInfo 供并行判断（解耦 provider 类型）
    let call_infos: Vec<parallel::ToolCallInfo> = tool_calls
        .iter()
        .map(|tc| parallel::ToolCallInfo {
            name: tc.name.clone(),
            arguments: tc.arguments.clone(),
        })
        .collect();

    if parallel::should_parallelize(&call_infos, &config) {
        tracing::info!(
            session_id = emitter.session_id(),
            count = tool_calls.len(),
            max_concurrent = config.max_concurrent,
            "工具批次并行执行"
        );
        execute_parallel(
            tool_calls,
            tools,
            agent_paths,
            emitter,
            result_tx,
            &config,
            cancel,
            subagent_ops,
            event_forwarder,
            todo_store,
        )
        .await
    } else {
        execute_sequential(
            tool_calls,
            tools,
            agent_paths,
            emitter,
            result_tx,
            cancel,
            subagent_ops,
            event_forwarder,
            todo_store,
        )
        .await
    }
}

/// 串行执行一批工具调用
///
/// 逐个查注册表 → 调 handler → 立即通过 `result_tx` 通知调用方。
/// 完成一个通知一个，不等全部跑完。
#[allow(clippy::too_many_arguments)]
async fn execute_sequential(
    tool_calls: &[ToolCallData],
    tools: &Arc<ToolRegistry>,
    agent_paths: &fuyao_api::AgentPaths,
    emitter: &Emitter,
    result_tx: &Sender<ToolExecResult>,
    cancel: &CancellationToken,
    subagent_ops: Option<std::sync::Weak<dyn fuyao_api::SubagentOps>>,
    event_forwarder: Option<UnboundedSender<OutputEvent>>,
    todo_store: Option<Arc<dyn TodoStoreOps>>,
) {
    let session_id = emitter.session_id().to_string();

    for tc in tool_calls {
        let result = execute_single(
            tc,
            tools,
            agent_paths,
            &session_id,
            cancel,
            subagent_ops.clone(),
            event_forwarder.clone(),
            todo_store.clone(),
        )
        .await;
        // 完成一个通知一个：调用方据此立即走 emit_to_history
        if result_tx.send(result).await.is_err() {
            tracing::warn!(
                session_id = emitter.session_id(),
                "result_tx 已关闭，工具结果丢弃"
            );
            return;
        }
    }
}

/// 并行执行一批工具调用
///
/// 使用 JoinSet + Semaphore 控制并发数。
/// - **通知顺序 = 完成顺序**：`join_next` 逐个收，完成即通过 `result_tx` 通知调用方
///   （UX 上调用方立即走 emit_to_history，UI 先看到先完成的工具结果）。
/// - **panic 隔离**：单个工具 task panic 产生 JoinError，降级为错误日志，不连坐兄弟任务。
///   JoinSet drop 时自动 abort 所有未完成任务（中断取消语义由调用方的 select! drop 触发）。
#[allow(clippy::too_many_arguments)]
async fn execute_parallel(
    tool_calls: &[ToolCallData],
    tools: &Arc<ToolRegistry>,
    agent_paths: &fuyao_api::AgentPaths,
    emitter: &Emitter,
    result_tx: &Sender<ToolExecResult>,
    config: &fuyao_api::ToolRunnerConfig,
    cancel: &CancellationToken,
    subagent_ops: Option<std::sync::Weak<dyn fuyao_api::SubagentOps>>,
    event_forwarder: Option<UnboundedSender<OutputEvent>>,
    todo_store: Option<Arc<dyn TodoStoreOps>>,
) {
    let semaphore = Arc::new(Semaphore::new(config.max_concurrent as usize));
    let session_id = emitter.session_id().to_string();
    let mut join_set: JoinSet<ToolExecResult> = JoinSet::new();

    for tc in tool_calls {
        let tc = tc.clone();
        let tools = tools.clone(); // Arc clone，廉价，多并行 task 共享同一注册表
        let agent_paths = agent_paths.clone(); // AgentPaths 已 Clone
        let session_id = session_id.clone();
        let semaphore = semaphore.clone();
        let cancel = cancel.clone(); // CancellationToken clone 廉价（Arc 共享），进 task 供 handler 监听
        let subagent_ops = subagent_ops.clone(); // Option<Weak> clone 廉价
        let event_forwarder = event_forwarder.clone(); // Option<Sender> clone 廉价
        let todo_store = todo_store.clone(); // Option<Arc> clone 廉价

        join_set.spawn(async move {
            // 获取许可：限制同一批次内同时运行的工具数（session 局部，不影响其他 session）
            let _permit = semaphore.acquire().await;
            execute_single(
                &tc,
                &tools,
                &agent_paths,
                &session_id,
                &cancel,
                subagent_ops,
                event_forwarder,
                todo_store,
            )
            .await
        });
    }

    // join_next 逐个收：完成一个通知一个（调用方据此 emit_to_history）
    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok(result) => {
                if result_tx.send(result).await.is_err() {
                    tracing::warn!(
                        session_id = emitter.session_id(),
                        "result_tx 已关闭，剩余工具结果丢弃"
                    );
                    return;
                }
            }
            Err(join_err) => {
                // task panic / 被取消：不连坐兄弟任务，降级为错误日志
                tracing::error!(
                    session_id = emitter.session_id(),
                    cause = %join_err,
                    "工具执行 task 异常"
                );
            }
        }
    }
}

/// 执行单个工具调用
///
/// 查注册表拿 handler → 解析参数 → 构建 `ToolCallContext` → 调 handler。
/// 容错：未知工具返回提示字符串；参数解析失败用 `Value::Null`。
///
/// 串行与并行共用本函数。`tools` 收 `&Arc<ToolRegistry>` 便于并行 task clone Arc。
/// `cancel` 是本次工具批次的中断信号，调 handler 时 clone 传入供其监听。
#[allow(clippy::too_many_arguments)]
async fn execute_single(
    tc: &ToolCallData,
    tools: &Arc<ToolRegistry>,
    agent_paths: &fuyao_api::AgentPaths,
    session_id: &str,
    cancel: &CancellationToken,
    subagent_ops: Option<std::sync::Weak<dyn fuyao_api::SubagentOps>>,
    event_forwarder: Option<UnboundedSender<OutputEvent>>,
    todo_store: Option<Arc<dyn TodoStoreOps>>,
) -> ToolExecResult {
    let tool_name = tc.name.clone();
    let tool_call_id = tc.id.clone();

    let Some(entry) = tools.get(&tool_name) else {
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

    // 构建上下文：注入 session_id + agent_paths + tool_call_id + subagent_ops + event_forwarder + todo_store
    // （工具据此识别会话、子代理类工具据此 upgrade 派生子 session + 转发子事件、
    //  todo 工具据此读写任务列表——其余工具按需取用）
    let ctx = ToolCallContext {
        session_id: Some(session_id.to_string()),
        agent_paths: Some(agent_paths.clone()),
        tool_call_id: Some(tool_call_id.clone()),
        subagent_ops,
        event_forwarder,
        todo_store,
    };

    let started = std::time::Instant::now();
    let content = (entry.handler)(args, ctx, cancel.clone()).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    tracing::info!(tool_name = %tool_name, elapsed_ms, "工具执行完成");

    ToolExecResult {
        tool_call_id,
        tool_name,
        content,
    }
}

#[cfg(test)]
mod tests;
