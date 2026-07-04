//! 工具运行子系统
//!
//! 管理工具调用的完整生命周期：
//! - 编排入口：调度 + 执行
//! - 执行器：串行/并行执行引擎
//!
//! 工具调用的拦截/观察统一通过 EventEmitter 处理。

pub(crate) mod executor;

use crate::engine::EventEmitter;
use fuyao_api::message::{EventBase, OutputEvent, ToolResultData};
use fuyao_api::{AgentContext, ToolFn};
use std::collections::HashMap;

/// 运行工具调用编排流程
///
/// # 流程
/// 1. 智能判断并行/串行执行
/// 2. 执行工具
/// 3. 推送 ToolResult 事件（通过 EventEmitter 统一发送）
pub(crate) async fn orchestrate(
    tool_calls: &[fuyao_provider::ToolCallData],
    tools_handlers: &HashMap<String, ToolFn>,
    agent_ctx: &AgentContext,
    emitter: &EventEmitter,
) {
    let config = &agent_ctx.tool_runner_config;

    if tool_calls.is_empty() {
        return;
    }

    let tool_results = executor::execute_tools(tool_calls, tools_handlers, agent_ctx, config).await;

    // 推送 ToolResult 事件
    for result in &tool_results {
        let event = OutputEvent::ToolResult(ToolResultData {
            base: EventBase::default(),
            tool_call_id: result.tool_call_id.clone(),
            tool_name: result.tool_name.clone(),
            content: result.content.clone(),
        });

        let _ = crate::dispatch::dispatch(event, None, emitter).await;
    }
}
