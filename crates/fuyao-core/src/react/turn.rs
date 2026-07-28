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
//!
//! shutdown：两段 select! 各有 `biased` 优先的 shutdown 分支（优先于 interrupt），
//! 命中后走与 interrupt 完全对称的四步链（emit_interrupt_event → classify →
//! handle_interrupt → persist），把已累积的部分结果落库后立即 return。
//! retry.rs 的退避 sleep 也监听 shutdown_token，收到信号立即冒泡 Cancelled
//! 让本层 shutdown 分支接管。这样 shutdown 不再依赖 10s abort 兜底。

use super::SessionCtx;
use super::builders::{
    ResolvedModel, assistant_msg_to_payload, assistant_with_tool_calls_to_payload,
    build_chat_request, resolve_model, tool_call_data_to_event, tool_call_event_to_data,
};
use crate::interrupt::{
    SharedTurnState, TurnState, classify, emit_interrupt_event, handle_interrupt,
};
use crate::react::queue;
use crate::stream::StreamResult;
use crate::tool_exec;
use fuyao_api::InterruptSource;
use fuyao_api::message::EventBase;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::output::{
    AssistantMessage, InterruptMessage as OutputInterruptMessage,
    InterruptPayload as OutputInterruptPayload, TitleMessage, TitlePayload,
};
use fuyao_api::{Message, MessageRole, ModelConfig, Session};
use fuyao_provider::Provider;
use std::sync::Arc;
use tokio::sync::mpsc::Receiver;

/// 构造 shutdown 中断 payload（供两段 select! 的 shutdown 分支复用）
///
/// shutdown 中断语义：source=Shutdown / reason="引擎关闭"。
/// 走与用户中断完全相同的 emit_interrupt_event + handle_interrupt 路径，
/// 让 UI 收到标准 Interrupt 事件，DB 记录能区分「引擎关闭中断」vs「用户主动中断」。
///
/// 内核内部产生的中断载荷本就是 output 侧类型，直接构造。
fn shutdown_interrupt_payload() -> OutputInterruptPayload {
    OutputInterruptPayload::new("引擎关闭", InterruptSource::Shutdown)
}

/// 运行一轮 ReAct（user messages 已由 run_session 主循环注入 session.messages）
///
/// `model_config` 取自 session 的 SessionParams 快照（决定 model/options），turn 内多轮复用。
/// `rx_interrupt` 为中断通道接收端，两段 select! 监听它。
pub(crate) async fn run_turn(
    ctx: &SessionCtx,
    session: &mut Session,
    rx_interrupt: &mut Receiver<OutputInterruptMessage>,
    model_config: ModelConfig,
) {
    // 解析本轮 model_id（含 None → [models.default] 兜底）+ 从 registry 查 Provider 实例
    // 任一失败：发 Error 事件 + 落库 + 结束本轮（配置错误，永久不可恢复）
    //
    // is_child 按 session.parent_session_id 判定：子 session 的工具列表过滤掉
    // child_invisible 的工具（递归防护——子 session 看不到派生类工具）
    let is_child = session.parent_session_id.is_some();
    let resolved: ResolvedModel = match resolve_model(&model_config, &ctx.tools, is_child) {
        Ok(r) => r,
        Err(msg) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                cause = %msg,
                "模型解析失败（model_id 无效或未配置 [models.default]）"
            );
            emit_config_error(ctx, &msg).await;
            persist(ctx.emitter.session_id(), session, &ctx.store).await;
            return;
        }
    };
    let provider: Arc<dyn Provider> = match ctx.providers.get(&resolved.provider_id) {
        Some(p) => p,
        None => {
            let msg = format!(
                "Provider '{}' 未注册（可用: {:?}）",
                resolved.provider_id,
                ctx.providers.provider_ids()
            );
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                cause = %msg,
                "Provider 实例未找到"
            );
            emit_config_error(ctx, &msg).await;
            persist(ctx.emitter.session_id(), session, &ctx.store).await;
            return;
        }
    };
    let model = resolved.model.clone();
    let options = resolved.options.clone();

    loop {
        // 本轮 LLM 调用的共享状态（中断分支读部分结果用）。
        // 每次 loop 顶部新建；retry.rs 在重试时清空复用，保证不携带上一次的部分结果。
        let state: SharedTurnState = Arc::new(std::sync::Mutex::new(TurnState::new()));
        // 本轮的 ChatRequest（重试间复用同一份——一次 LLM 调用内 DB 历史不变）
        // 消息已不在内存，每次构造时从 DB 查可见窗口
        let request = build_chat_request(
            ctx.store.as_ref(),
            ctx.emitter.session_id(),
            session.system_prompt.as_deref(),
        )
        .await;

        // 中断点①：流式期间（含重试 sleep 期间——select! drop future 即取消 sleep）
        // run_stream_with_retry 内部按错误类型自动重试，发 OutputEvent::Retry 给 UI。
        // 中断：外层 select! drop retry future → 退避 sleep 取消 → 中断分支胜出。
        // shutdown：retry.rs 退避 sleep 期间收到 shutdown 信号会冒泡 Cancelled，
        //          本 select! 的 shutdown 分支也并发监听 token，谁先到谁接管。
        let stream_result = {
            let retry_fut = super::retry::run_stream_with_retry(
                ctx, request, &model, &options, &provider, &state,
            );
            tokio::pin!(retry_fut);
            tokio::select! {
                biased;
                // shutdown 优先（高于 interrupt）：立即落库退出，不等流式结束
                _ = ctx.shutdown_token.cancelled() => {
                    let payload = shutdown_interrupt_payload();
                    emit_interrupt_event(&payload, &ctx.emitter, &ctx.hooks).await;
                    let kind = {
                        let s = state.lock().unwrap_or_else(|e| e.into_inner());
                        classify(&s)
                    };
                    handle_interrupt(&state, kind, &payload, &ctx.emitter, &ctx.hooks, ctx.store.as_ref(), session).await;
                    persist(ctx.emitter.session_id(), session, &ctx.store).await;
                    return;
                }
                result = &mut retry_fut => result,
                // 中断通道独立：此处只会收到 Interrupt
                interrupt_msg = rx_interrupt.recv() => {
                    if let Some(interrupt_msg) = interrupt_msg {
                        emit_interrupt_event(&interrupt_msg.payload, &ctx.emitter, &ctx.hooks).await;
                        let kind = {
                            let s = state.lock().unwrap_or_else(|e| e.into_inner());
                            classify(&s)
                        };
                        handle_interrupt(&state, kind, &interrupt_msg.payload, &ctx.emitter, &ctx.hooks, ctx.store.as_ref(), session).await;
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
                    handle_final_reply(ctx, session, &result, &model_config).await;
                    return;
                } else {
                    // 有工具调用：发 AssistantMessage → 执行整批工具 → 消费时机①
                    handle_tool_calls(ctx, session, rx_interrupt, &result, &model_config).await;
                    // execute_tools 内部若被中断会直接 return（见下方），此处 assume 已完成
                }
            }
            Err(e) => {
                // 错误冒泡（retry 已判定不可重试或耗尽）：
                // stream.rs 不再发 Error 事件（职责归位），由本层经 dispatch 发
                tracing::warn!(
                    session_id = ctx.emitter.session_id(),
                    cause = %e,
                    "LLM 调用失败，本轮未产出 Assistant 消息"
                );
                let error_event = OutputEvent::Error(fuyao_api::message::output::ErrorMessage {
                    base: EventBase::default(),
                    payload: fuyao_api::message::output::ErrorPayload {
                        message: format!("LLM 调用失败: {e}"),
                        recoverable: false,
                    },
                });
                crate::dispatch::dispatch(&ctx.emitter, &ctx.hooks, error_event, None).await;
                persist(ctx.emitter.session_id(), session, &ctx.store).await;
                return;
            }
        }
    }
}

/// 发"配置类错误"事件（永久不可恢复）
///
/// 统一处理 model_id 解析失败 / Provider 实例未注册等配置错误：发 `OutputEvent::Error`，
/// `recoverable: false`（与 LLM 调用失败共用 Error 通道，但 message 精准指向配置问题）。
async fn emit_config_error(ctx: &SessionCtx, message: &str) {
    let error_event = OutputEvent::Error(fuyao_api::message::output::ErrorMessage {
        base: EventBase::default(),
        payload: fuyao_api::message::output::ErrorPayload {
            message: message.to_string(),
            recoverable: false,
        },
    });
    crate::dispatch::dispatch(&ctx.emitter, &ctx.hooks, error_event, None).await;
}

/// 处理最终回复（AI 不调用工具，一轮 ReAct 结束）
///
/// 固定顺序：① pending 全部倒进 guide ② guide 全部消费注入 messages。
/// 都空 → turn 结束；有 → continue 回 run_turn 顶部再调一轮 LLM。
async fn handle_final_reply(
    ctx: &SessionCtx,
    session: &mut Session,
    result: &StreamResult,
    model_config: &ModelConfig,
) {
    // 回传本轮真实 usage 给主循环（pre-turn 压缩触发判定用）
    *ctx.last_usage.lock().await = Some(result.usage.clone());

    // 经 emit_to_history：拦截 → 闭包构造 Message（填 token + cost）→ 自动累积 session.total_* → 落 DB → 发送事件
    // 拦截不改 usage（token 是模型给的客观值），计费用原始 result.usage。
    let model_id = model_config.model_id.as_deref();
    let usage = result.usage.clone();
    let agent_paths = ctx.agent_paths.clone();
    let event = OutputEvent::Assistant(AssistantMessage {
        base: EventBase::default(),
        payload: assistant_msg_to_payload(result),
    });
    let _ = crate::dispatch::emit_to_history(
        &ctx.emitter,
        &ctx.hooks,
        ctx.store.as_ref(),
        session,
        event,
        |ev| match ev {
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
        },
    )
    .await;
    // 拦截 Block：消息不进历史、不计费——插件的责任，引擎不替它兜底

    // 消费时机②：① pending 全倒 guide ② guide 全取注入
    queue::drain_pending_to_guide(&ctx.guide, &ctx.pending);
    let msgs = queue::consume_all_guide(&ctx.guide);
    if msgs.is_empty() {
        // guide 和 pending 都空：落库，turn 结束
        persist(ctx.emitter.session_id(), session, &ctx.store).await;
        // 首轮最终回复后异步生成标题（fire-and-forget，不阻塞主循环）
        // 是否跳过子 session 由 [session.title] skip_child 控制（默认 true）
        let is_child = session.parent_session_id.is_some();
        maybe_spawn_title_generation(ctx, result, is_child).await;
    } else {
        // 有消息：全部注入（每条经 emit_to_history 拦截→落 DB→发送→观察），回 run_turn 顶部再调一轮 LLM
        queue::inject_messages(ctx, session, msgs).await;
    }
}

/// 首轮对话后触发标题自动生成（fire-and-forget）
///
/// 触发条件（同时满足）：
/// - `[session.title] enabled = true`
/// - 非子 session 或 `[session.title] skip_child = false`：子任务 session 用
///   `parent_session_id` 表达归属，重命名反而扰乱父/子分组与前端过滤
/// - DB 可见消息中 `role=user` 的消息数严格等于 1（首轮判定：计数法比
///   `title=="新会话"` 更稳——用户可能改过 title）
/// - 能取到首条 user content 与本轮 assistant 文本
///
/// 执行模型：`tokio::spawn` 独立 task，不阻塞主 ReAct 循环。
/// spawn 的 future 是 `'static` 的，**不借用 `&mut Session`**——标题直接走
/// `SessionStore::update_title` 单字段 SQL 落库，内存态不更新（下次 resume 时
/// 从 DB 自然读回）。
///
/// 多 session 并发天然安全：clone `Arc<store>` / `Arc<providers>` / `emitter` /
/// `hooks` / `agent_paths` 进 task，各 session task 独立，零共享零协调。
async fn maybe_spawn_title_generation(ctx: &SessionCtx, result: &StreamResult, is_child: bool) {
    let title_cfg = &fuyao_api::get_config().session.title;
    if !title_cfg.enabled {
        return;
    }

    // 子 session 跳过（可配置）：parent_session_id 已是归属标记，
    // 默认 skip_child=true 避免重命名扰乱父/子分组
    if is_child && title_cfg.skip_child {
        return;
    }

    // 从 DB 加载可见消息（事件级落库模式下消息不在内存）
    let visible = match ctx
        .store
        .load_visible_messages(ctx.emitter.session_id())
        .await
    {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                cause = %e,
                "标题生成前加载可见消息失败，跳过"
            );
            return;
        }
    };

    // 计数法判定首轮
    let user_count = visible
        .iter()
        .filter(|m| matches!(m.role, MessageRole::User))
        .count();
    if user_count != 1 {
        return;
    }

    // 取首条 user content + 本轮 assistant 文本（StreamResult.text 是本轮流式累积全文）
    let user_content = visible
        .iter()
        .find(|m| matches!(m.role, MessageRole::User))
        .and_then(|m| m.content.clone())
        .unwrap_or_default();
    let assistant_content = result.text.clone();
    // 标题生成回退用的主模型 ID：从可见消息最后一条 assistant 消息读取
    // （emit_to_history 闭包构造 Message 时已把 model_id 存进字段）。读不到则空串，
    // maybe_generate_title 内部会因 model_id 无法解析返回 None。
    let main_model_id = visible
        .iter()
        .rev()
        .find(|m| matches!(m.role, MessageRole::Assistant))
        .and_then(|m| m.model_id.clone())
        .unwrap_or_default();

    // clone 'static 依赖进 spawn（所有字段都是 Send + 'static）
    let store = Arc::clone(&ctx.store);
    let providers = Arc::clone(&ctx.providers);
    let emitter = ctx.emitter.clone();
    let hooks = ctx.hooks.clone();
    let agent_paths = ctx.agent_paths.clone();
    let session_id = ctx.emitter.session_id().to_string();

    tokio::spawn(async move {
        match fuyao_session::maybe_generate_title(
            &user_content,
            &assistant_content,
            &main_model_id,
            &providers,
            &agent_paths,
        )
        .await
        {
            Some(title) => {
                // 单字段落库（失败仅 warn，不影响主流程）
                if let Err(e) = store.update_title(&session_id, &title).await {
                    tracing::warn!(session_id = %session_id, cause = %e, "标题落库失败");
                    return;
                }
                // 发 Title 事件：经 dispatch 管道（拦截 → 发送 → 观察）
                crate::dispatch::dispatch(
                    &emitter,
                    &hooks,
                    OutputEvent::Title(TitleMessage {
                        base: EventBase::default(),
                        payload: TitlePayload { title },
                    }),
                    None,
                )
                .await;
            }
            None => tracing::debug!(session_id = %session_id, "标题生成跳过（无可用标题）"),
        }
    });
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
    rx_interrupt: &mut Receiver<OutputInterruptMessage>,
    result: &StreamResult,
    model_config: &ModelConfig,
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
    // 经 emit_to_history：拦截整个 AssistantMessage（同步 content/reasoning）→ 落 DB → 发送
    let effective_result = StreamResult {
        text: result.text.clone(),
        reasoning: result.reasoning.clone(),
        tool_calls: effective_tool_calls,
        usage: result.usage.clone(),
    };
    let model_id = model_config.model_id.as_deref();
    let usage = result.usage.clone();
    let agent_paths = ctx.agent_paths.clone();
    let event = OutputEvent::Assistant(AssistantMessage {
        base: EventBase::default(),
        payload: assistant_with_tool_calls_to_payload(&effective_result),
    });
    let _ = crate::dispatch::emit_to_history(
        &ctx.emitter,
        &ctx.hooks,
        ctx.store.as_ref(),
        session,
        event,
        |ev| match ev {
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
        },
    )
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

    // 步骤3：中断点②——工具执行期间（含 shutdown）
    // execute_tools 通过 result_tx 通知完成（一个一个通知）；本循环边收边走 emit_to_history
    // 中断/shutdown 时 channel 里已完成的也 push 进 messages（不丢），未完成的补发中断式 ToolResult。
    //
    // shutdown 与 interrupt 的落库逻辑完全相同（发 Interrupt 通知 → 补未完成 tool_result），
    // 抽 `emit_interrupt_and_complete_tool_results` 复用，唯一差异是 payload 的 source/reason。
    let tool_calls_for_exec = effective_result.tool_calls.clone();
    let (result_tx, mut result_rx) =
        tokio::sync::mpsc::channel::<tool_exec::ToolExecResult>(tool_calls_for_exec.len());
    // 派生 child_token：shutdown 时 parent→child 自动传播；interrupt 分支显式 cancel。
    // handler 据此优雅收尾长任务（杀子进程等），不监听的靠 abort 兜底（双保险）
    let cancel = ctx.shutdown_token.child_token();
    let exec_fut = tool_exec::execute_tools(
        &tool_calls_for_exec,
        &ctx.tools,
        &ctx.agent_paths,
        &ctx.emitter,
        &result_tx,
        &cancel,
        ctx.subagent_ops.clone(),
    );
    tokio::pin!(exec_fut);

    loop {
        tokio::select! {
            biased; // shutdown / 中断优先，保证及时响应
            // shutdown 优先（高于 interrupt）：立即清空已完成工具结果 + 落库退出
            // 工具执行 fut 被 drop → JoinSet drop → tokio 自动 abort 所有未完成工具 task
            _ = ctx.shutdown_token.cancelled() => {
                // 清空 channel 把已完成的 push 进 messages（不丢已完成结果）
                while let Ok(r) = result_rx.try_recv() {
                    push_tool_result_to_history(ctx, session, r).await;
                }
                emit_interrupt_and_complete_tool_results(
                    ctx, session, &effective_result.tool_calls,
                    &shutdown_interrupt_payload(),
                ).await;
                persist(ctx.emitter.session_id(), session, &ctx.store).await;
                return;
            }
            cmd = rx_interrupt.recv() => {
                // interrupt 命中：显式 cancel 工具批 child_token（shutdown 靠 parent 传播，无需此处 cancel）
                // 让监听 token 的长任务 handler 后台优雅收尾；无宽限期，立即清空 + 补发
                cancel.cancel();
                // 收到 Interrupt 或通道关闭（None）：清空 channel 把已完成的 push 进 messages
                // 用 try_recv 非阻塞清空（exec_fut 可能还在跑，recv 会阻塞）
                while let Ok(r) = result_rx.try_recv() {
                    push_tool_result_to_history(ctx, session, r).await;
                }
                if let Some(ref interrupt_msg) = cmd {
                    emit_interrupt_and_complete_tool_results(
                        ctx, session, &effective_result.tool_calls,
                        &interrupt_msg.payload,
                    ).await;
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
///
/// 消息已在产生时经 emit_to_history → insert_message 落库，本函数只同步 sessions
/// 表的元数据（统计字段、ended_at 等）。message_count 由 emit_to_history 维护
/// 内存计数器，update 时自然同步到 DB。
async fn persist(session_id: &str, session: &Session, store: &Arc<fuyao_session::SessionStore>) {
    if let Err(e) = store.update(session).await {
        tracing::warn!(session_id = session_id, cause = %e, "session 元数据落库失败");
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

/// 把工具执行结果经 emit_to_history 单条落 DB（拦截后构造 Message）
///
/// 工具完成时立即调用：拦截 → `insert_message` 落 DB（Message::tool_result）→ 发送事件 → 观察。
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

/// 把预构造的 ToolResult 事件经 emit_to_history 单条落 DB
///
/// 用于中断补发：事件由调用方构造（content 标记中断原因），拦截后落 DB（Message::tool_result）。
async fn push_tool_result_event_to_history(
    ctx: &SessionCtx,
    session: &mut Session,
    event: OutputEvent,
) {
    let _ = crate::dispatch::emit_to_history(
        &ctx.emitter,
        &ctx.hooks,
        ctx.store.as_ref(),
        session,
        event,
        |ev| match ev {
            OutputEvent::ToolResult(m) => Some(Message::tool_result(
                m.payload.tool_call_id.clone(),
                m.payload.content.clone(),
            )),
            _ => None,
        },
    )
    .await;
}

/// 发 Interrupt 通知 + 为未完成 tool_call 补发中断式 ToolResult（落 DB）
///
/// 工具执行期间收到 interrupt 或 shutdown 信号时复用本函数（payload 由调用方决定）。
/// 调用方应在调用本函数前**先用 `try_recv` 清空 channel** 把已完成的工具结果 push 进 history，
/// 本函数只负责「通知 + 补未完成」两步。
///
/// 未完成判定：从 DB 查询已落库的 answered tool_call_id（事件级落库模式下消息不在内存），
/// effective 中不在 answered 集合的 tool_call 视为未完成，逐个补发中断式 ToolResult。
async fn emit_interrupt_and_complete_tool_results(
    ctx: &SessionCtx,
    session: &mut Session,
    effective_tool_calls: &[fuyao_provider::ToolCallData],
    payload: &OutputInterruptPayload,
) {
    emit_interrupt_event(payload, &ctx.emitter, &ctx.hooks).await;

    // 从 DB 查询已落库的 answered tool_call_id（事件级落库模式下消息不在内存）
    let answered: std::collections::HashSet<String> = match ctx
        .store
        .load_visible_messages(ctx.emitter.session_id())
        .await
    {
        Ok(msgs) => msgs
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .filter_map(|m| m.tool_call_id.clone())
            .collect(),
        Err(e) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                cause = %e,
                "中断补发前加载可见消息失败，按全部未完成处理"
            );
            std::collections::HashSet::new()
        }
    };

    // 为 effective 中未完成的 tool_call 补发中断式 ToolResult（也走 emit_to_history）
    for tc in effective_tool_calls {
        if !answered.contains(&tc.id) {
            let ev = make_interrupt_tool_result_event(
                tc.id.clone(),
                tc.name.clone(),
                &payload.source,
                &payload.reason,
            );
            push_tool_result_event_to_history(ctx, session, ev).await;
        }
    }
}
