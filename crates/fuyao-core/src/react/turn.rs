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
    assistant_msg_to_payload, assistant_with_tool_calls_to_payload, build_assistant_message,
    build_assistant_message_with_tool_calls, build_chat_request, build_model_and_options,
    tool_call_data_to_event, tool_call_event_to_data,
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
                        handle_interrupt(&state, kind, &interrupt_msg.payload, &ctx.emitter, &ctx.hooks).await;
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

    // 发最终 AssistantMessage（经管道：拦截 → 发送 → 观察）
    let assistant_msg = build_assistant_message(result, params.model_config.model_id.as_deref());
    session.messages.push(assistant_msg);
    crate::dispatch::dispatch(
        &ctx.emitter,
        &ctx.hooks,
        OutputEvent::Assistant(AssistantMessage {
            base: EventBase::default(),
            payload: assistant_msg_to_payload(result),
        }),
        None,
    )
    .await;

    // 消费时机②：① pending 全倒 guide ② guide 全取注入
    queue::drain_pending_to_guide(&ctx.guide, &ctx.pending);
    let msgs = queue::consume_all_guide(&ctx.guide);
    if msgs.is_empty() {
        // guide 和 pending 都空：落库，turn 结束
        persist(ctx.emitter.session_id(), session, &ctx.store).await;
    } else {
        // 有消息：全部注入，回 run_turn 顶部再调一轮 LLM
        queue::inject_messages(session, msgs);
    }
}

/// 处理工具调用：逐个拦截工具调用 → 发 AssistantMessage → 执行整批工具 → 消费时机①
///
/// 工具调用逐个经 dispatch_intercept 拦截（照搬归档做法）：
/// - 每个工具调用作为一条 `OutputEvent::ToolCall` 事件单独拦截
/// - 插件可修改其参数/名称，或返回 Block 跳过该工具
/// - 未被 Block 的累积成 `effective_tool_calls`，作为后续「存储 / 发送 / 执行」的唯一数据源
///
/// 一批工具全部执行完成后才消费 guide（不是每个工具完成都消费）。
async fn handle_tool_calls(
    ctx: &SessionCtx,
    session: &mut Session,
    rx_interrupt: &mut Receiver<InterruptMessage>,
    result: &StreamResult,
    params: &MessageParams,
) {
    // 步骤1：逐个拦截工具调用，构造 effective_tool_calls
    // 整批 tool_calls 拆成单个 ToolCall 事件，各自经管道拦截；Block 的跳过。
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

    // 步骤2：用 effective_tool_calls 构造存储 Message + 发送 AssistantMessage
    // 拦截后的结果作为唯一数据源：存储 / 发送 / 执行三者一致。
    // usage 由模型给出，拦截不改变它，沿用原始结果即可。
    let effective_result = StreamResult {
        text: result.text.clone(),
        reasoning: result.reasoning.clone(),
        tool_calls: effective_tool_calls,
        usage: result.usage.clone(),
    };
    let assistant_msg = build_assistant_message_with_tool_calls(
        &effective_result,
        params.model_config.model_id.as_deref(),
    );
    session.messages.push(assistant_msg);
    crate::dispatch::dispatch(
        &ctx.emitter,
        &ctx.hooks,
        OutputEvent::Assistant(AssistantMessage {
            base: EventBase::default(),
            payload: assistant_with_tool_calls_to_payload(&effective_result),
        }),
        None,
    )
    .await;

    // 若全部工具调用被拦截（effective 为空），无需执行，直接走消费时机①
    if effective_result.tool_calls.is_empty() {
        let msgs = queue::consume_all_guide(&ctx.guide);
        if !msgs.is_empty() {
            queue::inject_messages(session, msgs);
        }
        return;
    }

    // 步骤3：中断点②——工具执行期间
    // 中断（Interrupt）或通道关闭（None，session 结束）都取消工具执行、落库结束 turn。
    // select! 命中 recv 分支时 exec_fut 被 drop，未完成的工具结果丢失（已完成的已发出）。
    let tool_results = {
        let exec_fut = tool_exec::execute_tools(
            &effective_result.tool_calls,
            &ctx.tools,
            &ctx.agent_paths,
            &ctx.emitter,
            &ctx.hooks,
        );
        tokio::select! {
            results = exec_fut => results,
            cmd = rx_interrupt.recv() => {
                // 收到 Interrupt 或通道关闭（None）：发中断/补发 ToolResult，落库结束
                if let Some(ref interrupt_msg) = cmd {
                    emit_interrupt_event(&interrupt_msg.payload, &ctx.emitter, &ctx.hooks).await;
                    // 为所有 effective tool_calls 发中断式 ToolResult
                    // （execute_tools 完成一个发出一个，已完成的已发；
                    //  select! drop exec_fut 时未完成的丢失，这里统一补发）
                    for tc in &effective_result.tool_calls {
                        crate::dispatch::dispatch(
                            &ctx.emitter,
                            &ctx.hooks,
                            make_interrupt_tool_result_event(
                                tc.id.clone(),
                                tc.name.clone(),
                                &interrupt_msg.payload.source,
                                &interrupt_msg.payload.reason,
                            ),
                            None,
                        )
                        .await;
                    }
                }
                persist(ctx.emitter.session_id(), session, &ctx.store).await;
                return;
            }
        }
    };

    // 工具结果进 task 本地 messages（不走队列）
    for tr in &tool_results {
        session.messages.push(Message::tool_result(
            tr.tool_call_id.clone(),
            tr.content.clone(),
        ));
    }

    // 步骤4：消费时机①——一批工具全部完成后、发回 AI 前，只看 guide（pending 不动）
    let msgs = queue::consume_all_guide(&ctx.guide);
    if !msgs.is_empty() {
        queue::inject_messages(session, msgs);
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
