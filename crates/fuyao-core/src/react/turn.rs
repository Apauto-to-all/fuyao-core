//! 单轮 ReAct 循环
//!
//! 一个 turn = 处理一批已注入的 user messages，驱动"想 → 调工具 → 再想"循环，
//! 直到 AI 不再调工具（最终回复）且 guide/pending 都空才结束。
//!
//! 两个消费时机：
//! - **一批工具全部执行完成后、发回 AI 前**：只看 guide（还在调工具，pending 不动）。
//!   guide 全取注入 → continue；guide 空 → continue（只带工具结果）。
//! - **AI 不调用工具（最终回复，一轮 ReAct 结束）**：固定顺序
//!   ① pending 全部倒进 guide ② guide 全部消费注入 messages → 都空才结束 turn。
//!
//! 中断：两段 select!——流式期间、工具执行期间。idle 段在 run_session 外层。
//! 中断时保存部分结果（发增量事件），落库，结束本轮。

use super::SessionCtx;
use super::builders::{
    assistant_msg_to_payload, assistant_with_tool_calls_to_payload, build_chat_request,
    build_model_and_options, tool_call_data_to_event, tool_call_event_to_data,
};
use crate::interrupt::{
    SharedTurnState, TurnState, classify, emit_interrupt_event, handle_interrupt,
};
use crate::react::queue;
use crate::stream::{self, StreamResult};
use crate::tool_exec;
use fuyao_api::message::EventBase;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::input::InterruptMessage;
use fuyao_api::message::output::AssistantMessage;
use fuyao_api::{Message, MessageParams, Session};
use fuyao_provider::StreamDecoder;
use std::sync::Arc;
use tokio::sync::mpsc::Receiver;

/// 运行一轮 ReAct（user messages 已由 run_session 主循环注入 session.messages）
///
/// `params` 取自本轮 guide 第一条消息（决定 model/options），turn 内多轮复用。
/// `rx_interrupt` 为中断通道接收端，两段 select! 监听它。
pub(crate) async fn run_turn(
    ctx: &SessionCtx,
    session: &mut Session,
    rx_interrupt: &mut Receiver<InterruptMessage>,
    params: MessageParams,
) {
    let (model, options) = build_model_and_options(&params, &ctx.tools);

    loop {
        let request = build_chat_request(session);
        let mut decoder = StreamDecoder::new();
        let state: SharedTurnState = Arc::new(std::sync::Mutex::new(TurnState::new()));

        // 中断点①：流式期间
        let stream_result = {
            let stream_fut = stream::run_stream_session(
                request,
                &model,
                options.clone(),
                &ctx.provider,
                &ctx.emitter,
                &ctx.hooks,
                &mut decoder,
                &state,
            );
            tokio::pin!(stream_fut);
            tokio::select! {
                result = &mut stream_fut => result,
                // 中断通道独立：此处只会收到 Interrupt
                interrupt_msg = rx_interrupt.recv() => {
                    if let Some(interrupt_msg) = interrupt_msg {
                        emit_interrupt_event(&interrupt_msg.payload, &ctx.emitter, &ctx.hooks).await;
                        let kind = {
                            let s = state.lock().unwrap_or_else(|e| e.into_inner());
                            classify(&s)
                        };
                        handle_interrupt(&state, kind, &interrupt_msg.payload, &ctx.emitter, &ctx.hooks, session).await;
                        persist(ctx.emitter.session_id(), session, &ctx.store).await;
                        return;
                    }
                    // 中断通道关闭：忽略，继续等流式
                    continue;
                }
            }
        };

        match stream_result {
            Ok(result) => {
                if result.tool_calls.is_empty() {
                    // 无工具调用：最终回复
                    handle_final_reply(ctx, session, &result, &params).await;
                    return;
                } else {
                    // 有工具调用：发 AssistantMessage → 执行整批工具 → 消费时机①
                    handle_tool_calls(ctx, session, rx_interrupt, &result, &params).await;
                    // execute_tools 内部若被中断会直接 return（见下方），此处 assume 已完成
                }
            }
            Err(_) => {
                tracing::warn!(
                    session_id = ctx.emitter.session_id(),
                    "LLM 调用失败，本轮未产出 Assistant 消息"
                );
                persist(ctx.emitter.session_id(), session, &ctx.store).await;
                return;
            }
        }
    }
}

/// 处理最终回复（AI 不调用工具，一轮 ReAct 结束）
///
/// 固定顺序：① pending 全部倒进 guide ② guide 全部消费注入 messages。
/// 都空 → turn 结束；有 → continue 回 run_turn 顶部再调一轮 LLM。
async fn handle_final_reply(
    ctx: &SessionCtx,
    session: &mut Session,
    result: &StreamResult,
    params: &MessageParams,
) {
    // 回传本轮真实 usage 给主循环（pre-turn 压缩触发判定用）
    *ctx.last_usage.lock().await = Some(result.usage.clone());

    // 经 emit_to_history：拦截 → 闭包构造 Message（填 token + cost）→ 自动累积 session.total_* → push → 发送事件
    // 拦截不改 usage（token 是模型给的客观值），计费用原始 result.usage。
    let model_id = params.model_config.model_id.as_deref();
    let usage = result.usage.clone();
    let agent_paths = ctx.agent_paths.clone();
    let event = OutputEvent::Assistant(AssistantMessage {
        base: EventBase::default(),
        payload: assistant_msg_to_payload(result),
    });
    let _ =
        crate::dispatch::emit_to_history(&ctx.emitter, &ctx.hooks, session, event, |ev| match ev {
            OutputEvent::Assistant(m) => {
                let mut msg = Message::assistant(m.payload.content.clone());
                msg.reasoning = m.payload.reasoning.clone();
                msg.model_id = model_id.map(|s| s.to_string());
                msg.finish_reason = Some("stop".to_string());
                // 填 token + cost（拦截不改 usage）——统一调 session 模块
                fuyao_session::fill_message_cost(&mut msg, &usage, model_id, &agent_paths);
                Some(msg)
            }
            _ => None,
        })
        .await;
    // 拦截 Block：消息不进历史、不计费——插件的责任，引擎不替它兜底

    // 消费时机②：① pending 全倒 guide ② guide 全取注入
    queue::drain_pending_to_guide(&ctx.guide, &ctx.pending);
    let msgs = queue::consume_all_guide(&ctx.guide);
    if msgs.is_empty() {
        // guide 和 pending 都空：落库，turn 结束
        persist(ctx.emitter.session_id(), session, &ctx.store).await;
    } else {
        // 有消息：全部注入（每条经 emit_to_history 拦截→push→发送→观察），回 run_turn 顶部再调一轮 LLM
        queue::inject_messages(ctx, session, msgs).await;
    }
}

/// 处理工具调用：逐个拦截工具调用 → emit_to_history 同步 AssistantMessage → 执行整批工具 → 消费时机①
///
/// 工具调用两层拦截模型（清晰边界）：
/// - **第一层：ToolCall 事件逐个拦截**（粒度细）：插件可独立 Block 单个 tool_call 或改其 args。
///   拦截后的 effective_tool_calls 作为「执行输入」+「存储字段」的权威数据源。
/// - **第二层：AssistantMessage 事件整体拦截**（粒度粗）：插件可改 content / reasoning
///   等内容字段。但 tool_calls 字段**以 effective_tool_calls 为准**——避免「存储用的 tool_calls」
///   与「执行的 tool_calls」分裂。如需改 tool_calls，请在第一层 ToolCall 拦截时改。
///
/// emit_to_history 统一入口：拦截 → push session.messages → 发送事件 → 观察。
/// 工具执行通过 channel 通知完成，turn.rs 边收边走 emit_to_history（拦截 → push messages → 发事件）。
/// 中断时 channel 里剩余结果也清空 push，保证不丢。
async fn handle_tool_calls(
    ctx: &SessionCtx,
    session: &mut Session,
    rx_interrupt: &mut Receiver<InterruptMessage>,
    result: &StreamResult,
    params: &MessageParams,
) {
    // 步骤1：逐个拦截 ToolCall 事件，构造 effective_tool_calls
    // 整批 tool_calls 拆成单个 ToolCall 事件各自拦截；Block 的跳过。
    let mut effective_tool_calls: Vec<fuyao_provider::ToolCallData> =
        Vec::with_capacity(result.tool_calls.len());
    for tc in &result.tool_calls {
        let event = tool_call_data_to_event(tc);
        if let Some(intercepted) =
            crate::dispatch::dispatch_intercept(&ctx.emitter, &ctx.hooks, event).await
        {
            // 拦截 Pass：发送（含观察），并从拦截后的 payload 提取工具调用数据回灌
            crate::dispatch::deliver(&ctx.emitter, &ctx.hooks, intercepted.clone()).await;
            if let Some(data) = tool_call_event_to_data(&intercepted) {
                effective_tool_calls.push(data);
            }
        }
        // Block：跳过该工具（不发送、不执行、不存储）
    }

    // 步骤2：用 effective_tool_calls 构造 effective_result → AssistantMessage 事件
    // 经 emit_to_history：拦截整个 AssistantMessage（同步 content/reasoning）→ push messages → 发送
    let effective_result = StreamResult {
        text: result.text.clone(),
        reasoning: result.reasoning.clone(),
        tool_calls: effective_tool_calls,
        usage: result.usage.clone(),
    };
    let model_id = params.model_config.model_id.as_deref();
    let usage = result.usage.clone();
    let agent_paths = ctx.agent_paths.clone();
    let event = OutputEvent::Assistant(AssistantMessage {
        base: EventBase::default(),
        payload: assistant_with_tool_calls_to_payload(&effective_result),
    });
    let _ =
        crate::dispatch::emit_to_history(&ctx.emitter, &ctx.hooks, session, event, |ev| match ev {
            OutputEvent::Assistant(m) => {
                // tool_calls 字段以 effective_result.tool_calls（已拦截 ToolCall 事件）为准
                let tool_calls_json: Vec<serde_json::Value> = effective_result
                    .tool_calls
                    .iter()
                    .map(|tc| {
                        serde_json::json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {"name": tc.name, "arguments": tc.arguments}
                        })
                    })
                    .collect();
                let mut msg = Message::assistant(m.payload.content.clone());
                msg.reasoning = m.payload.reasoning.clone();
                if !tool_calls_json.is_empty() {
                    msg.tool_calls = Some(serde_json::Value::Array(tool_calls_json));
                }
                msg.model_id = model_id.map(|s| s.to_string());
                msg.finish_reason = Some("tool_calls".to_string());
                // 填 token + cost（拦截不改 usage）——统一调 session 模块
                fuyao_session::fill_message_cost(&mut msg, &usage, model_id, &agent_paths);
                Some(msg)
            }
            _ => None,
        })
        .await;
    // 拦截 Block：消息不进历史、不计费——插件的责任

    // 若全部工具调用被拦截（effective 为空）或 AssistantMessage 被 Block，无需执行
    if effective_result.tool_calls.is_empty() {
        let msgs = queue::consume_all_guide(&ctx.guide);
        if !msgs.is_empty() {
            queue::inject_messages(ctx, session, msgs).await;
        }
        return;
    }

    // 步骤3：中断点②——工具执行期间
    // execute_tools 通过 result_tx 通知完成（一个一个通知）；本循环边收边走 emit_to_history
    // 中断时 channel 里已完成的也 push 进 messages（不丢），未完成的补发中断式 ToolResult。
    let tool_calls_for_exec = effective_result.tool_calls.clone();
    let (result_tx, mut result_rx) =
        tokio::sync::mpsc::channel::<tool_exec::ToolExecResult>(tool_calls_for_exec.len());
    let exec_fut = tool_exec::execute_tools(
        &tool_calls_for_exec,
        &ctx.tools,
        &ctx.agent_paths,
        &ctx.emitter,
        &result_tx,
    );
    tokio::pin!(exec_fut);

    loop {
        tokio::select! {
            biased; // 中断优先，保证及时响应
            cmd = rx_interrupt.recv() => {
                // 收到 Interrupt 或通道关闭（None）：清空 channel 把已完成的 push 进 messages
                // 用 try_recv 非阻塞清空（exec_fut 可能还在跑，recv 会阻塞）
                while let Ok(r) = result_rx.try_recv() {
                    push_tool_result_to_history(ctx, session, r).await;
                }
                if let Some(ref interrupt_msg) = cmd {
                    emit_interrupt_event(&interrupt_msg.payload, &ctx.emitter, &ctx.hooks).await;
                    // 为 effective 中未完成的 tool_call 补发中断式 ToolResult（也走 emit_to_history）
                    let answered: std::collections::HashSet<String> = session
                        .messages
                        .iter()
                        .filter(|m| m.role == "tool")
                        .filter_map(|m| m.tool_call_id.clone())
                        .collect();
                    for tc in &effective_result.tool_calls {
                        if !answered.contains(&tc.id) {
                            let ev = make_interrupt_tool_result_event(
                                tc.id.clone(),
                                tc.name.clone(),
                                &interrupt_msg.payload.source,
                                &interrupt_msg.payload.reason,
                            );
                            push_tool_result_event_to_history(ctx, session, ev).await;
                        }
                    }
                }
                persist(ctx.emitter.session_id(), session, &ctx.store).await;
                return;
            }
            Some(r) = result_rx.recv() => {
                // 完成一个：立即走 emit_to_history（拦截 → push messages → 发送事件）
                push_tool_result_to_history(ctx, session, r).await;
            }
            _ = &mut exec_fut => {
                // execute_tools 完成：清空 channel 里剩余的（防丢，理论已空）
                while let Ok(r) = result_rx.try_recv() {
                    push_tool_result_to_history(ctx, session, r).await;
                }
                break;
            }
        }
    }

    // 步骤4：消费时机①——一批工具全部完成后、发回 AI 前，只看 guide（pending 不动）
    let msgs = queue::consume_all_guide(&ctx.guide);
    if !msgs.is_empty() {
        queue::inject_messages(ctx, session, msgs).await;
    }
    // 回 run_turn 顶部：带 guide 消息（若有）+ 工具结果再调 LLM
}

/// 落库（边界时刻调用）
async fn persist(
    session_id: &str,
    session: &mut Session,
    store: &Arc<fuyao_session::SessionStore>,
) {
    session.message_count = session.messages.len() as i64;
    if let Err(e) = store.update(session).await {
        tracing::warn!(session_id = session_id, cause = %e, "session 落库失败");
    }
}

/// 构建中断式 ToolResult 事件
fn make_interrupt_tool_result_event(
    tool_call_id: String,
    tool_name: String,
    source: &fuyao_api::InterruptSource,
    reason: &str,
) -> OutputEvent {
    OutputEvent::ToolResult(fuyao_api::message::output::ToolResultMessage {
        base: EventBase::default(),
        payload: fuyao_api::message::output::ToolResultPayload {
            tool_call_id,
            tool_name,
            content: format!("[{source:?}][{reason}]"),
        },
    })
}

/// 把工具执行结果经 emit_to_history 推进 session.messages（拦截后构造 Message）
///
/// 工具完成时立即调用：拦截 → push Message::tool_result → 发送事件 → 观察。
/// 保证「拦截→存储→发送」三者一致；中断时已完成的也不丢。
async fn push_tool_result_to_history(
    ctx: &SessionCtx,
    session: &mut Session,
    result: tool_exec::ToolExecResult,
) {
    let event = OutputEvent::ToolResult(fuyao_api::message::output::ToolResultMessage {
        base: EventBase::default(),
        payload: fuyao_api::message::output::ToolResultPayload {
            tool_call_id: result.tool_call_id,
            tool_name: result.tool_name,
            content: result.content,
        },
    });
    push_tool_result_event_to_history(ctx, session, event).await;
}

/// 把预构造的 ToolResult 事件经 emit_to_history 推进 session.messages
///
/// 用于中断补发：事件由调用方构造（content 标记中断原因），拦截后 push Message::tool_result。
async fn push_tool_result_event_to_history(
    ctx: &SessionCtx,
    session: &mut Session,
    event: OutputEvent,
) {
    let _ =
        crate::dispatch::emit_to_history(&ctx.emitter, &ctx.hooks, session, event, |ev| match ev {
            OutputEvent::ToolResult(m) => Some(Message::tool_result(
                m.payload.tool_call_id.clone(),
                m.payload.content.clone(),
            )),
            _ => None,
        })
        .await;
}
