//! ReAct 循环 task（session 执行流）
//!
//! 每个活跃 session 独立运行一个 task，消费 guide 队列，驱动 ReAct 循环：
//! 想（LLM）→ 可能调一批工具 → 全部工具完成后消费 guide → 再想 → ... → 最终回复。
//!
//! 并发模型：多 session 各自一个 task，tokio 调度，异步并发。
//! 同 session 内单 task 串行（处理完一个 turn 才取下一个）。
//!
//! 双队列（guide / pending）：
//! - guide：直接消费的队列，触发消费时机时一次性全部取出，每条变一条 user message
//!   注入 session.messages，回循环顶部调 LLM
//! - pending：排队队列，AI 不再调工具（最终回复）后才一次性全部倒进 guide
//!
//! 两个消费时机（详见 [`turn::run_turn`]）：
//! - 一批工具全部执行完成后、发回 AI 前：只看 guide（还在调工具，pending 不动）
//! - AI 不调用工具（最终回复，一轮 ReAct 结束）：先 pending 全倒 guide，再 guide 全消费
//!
//! 中断通道与队列分离：Interrupt 走独立 `rx_interrupt`（mpsc），
//! select! 中断点只监听它——不会误取 User/Plugin。
//!
//! 工具结果不走队列：它是 ReAct 循环内部中间产物，直接 push 进 session.messages。

mod builders;
pub(crate) mod queue;
#[cfg(test)]
mod tests;
pub(crate) mod turn;

use crate::dispatch;
use crate::emit::Emitter;
use crate::engine::types::{QueuedUserMessage, SharedQueue};
use crate::interrupt::emit_interrupt_event;
use crate::tool_registry::ToolRegistry;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::input::{InterruptMessage, PluginMessage};
use fuyao_api::message::output::{
    PluginMessage as OutputPluginMessage, PluginPayload as OutputPluginPayload,
    UserMessage as OutputUserMessage, UserPayload,
};
use fuyao_api::{CompressionConfig, InboundUser, Session};
use fuyao_api::{UserMessageMode, UserMessageSource};
use fuyao_hooks::SharedHooks;
use fuyao_provider::{Provider, StreamUsage};
use fuyao_session::CompressionRuntimeState;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc::{Receiver, Sender};

/// session 的共享依赖（引擎级共享能力的 owned 视图）
///
/// 聚合 store / provider / tools / hooks / agent_paths / emitter / guide / pending
/// 这些所有 turn 都需要的共享只读依赖 + 双队列，避免 run_turn 参数列表过长。
/// 不含可变状态（session / rx_interrupt）——那些作为独立 &mut 参数传入。
/// 由 run_session 构造一次，整个 task 期间以 `&SessionCtx` 不可变借用复用。
pub(crate) struct SessionCtx {
    pub store: Arc<fuyao_session::SessionStore>,
    pub provider: Arc<dyn Provider>,
    pub tools: Arc<ToolRegistry>,
    /// 钩子注册表（引擎级共享，透传给本 session 的 dispatch 管道）
    pub hooks: SharedHooks,
    pub agent_paths: fuyao_api::AgentPaths,
    /// Agent 配置（创建时定死的会话级配置，压缩后重建 system_prompt 用）
    pub agent_config: fuyao_api::AgentConfig,
    pub emitter: Emitter,
    /// 引导队列（直接消费）
    pub guide: SharedQueue,
    /// 排队队列（最终回复后转入 guide）
    pub pending: SharedQueue,
    /// 上一轮 LLM 返回的真实 usage（pre-turn 压缩触发判定用）
    ///
    /// 由 [`turn::handle_final_reply`] 写入，主循环 pre-turn 读。None 表示首轮尚未跑过。
    pub last_usage: Arc<Mutex<Option<StreamUsage>>>,
    /// 压缩运行时状态（反抖动统计，per-session）
    pub compression_state: Arc<std::sync::Mutex<CompressionRuntimeState>>,
    /// 压缩配置（从全局 config 读取，启动时定死）
    pub compression_config: CompressionConfig,
}

/// session 的独立执行流
///
/// 消费 guide 队列驱动 ReAct 循环；guide 空时 select! 等待入站消息（过管道入队）
/// 或 rx_interrupt（idle 中断）。
///
/// User 消息统一经管道处理：Engine::send 把消息送入站通道 → select! 收到 →
/// 过完整管道（拦截 → 处理[入 guide/pending 队列] → 发送[回显 User] → 观察）。
/// 入队是 session 层管道的 process 职责，不在引擎层直接操作队列。
///
/// 关于 SessionParams 的简化（有意决策）：`SessionParams` 在 `Engine::create_session`
/// 里被消费——只取出 `agent_config` 构建 system_prompt 存进 `Session.system_prompt`，
/// 之后 SessionParams 本身不再传入 task。task 运行时需要的配置走两条路：
/// - 工具配置：引擎级 `ToolRegistry` 共享（`tools` 参数），启动时装配。
/// - session 级配置（system_prompt）：已构建进 `Session`，task 直接读。
///
/// 这样多 session 并发时工具共享、人格隔离，互不干扰。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_session(
    session_id: String,
    guide: SharedQueue,
    pending: SharedQueue,
    mut rx_inbound: Receiver<InboundUser>,
    mut rx_interrupt: Receiver<InterruptMessage>,
    mut rx_plugin: Receiver<PluginMessage>,
    mut session: Session,
    store: Arc<fuyao_session::SessionStore>,
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    hooks: SharedHooks,
    agent_paths: fuyao_api::AgentPaths,
    agent_config: fuyao_api::AgentConfig,
    tx_event: Sender<OutputEvent>,
) {
    tracing::info!(session_id = %session_id, "session 执行流启动");

    let ctx = SessionCtx {
        store,
        provider,
        tools,
        hooks,
        agent_paths,
        agent_config,
        emitter: Emitter::new(tx_event, session_id.clone()),
        guide,
        pending,
        last_usage: Arc::new(Mutex::new(None)),
        compression_state: Arc::new(std::sync::Mutex::new(CompressionRuntimeState::default())),
        compression_config: fuyao_api::get_config().session.compression.clone(),
    };

    // 主循环：从 guide 全取消息 → 注入 → 跑一轮 ReAct；guide 空 → 等待入站/中断
    loop {
        // task 空闲时（无活跃 turn）= 无进行中的 ReAct 链，pending 的"等链结束"解禁条件已满足
        // → 此时 pending 与 guide 语义等价，立即解禁进 guide 触发新 turn
        // （否则只发 pending 时 pending 会死信，永远进不了 turn）
        let mut msgs = queue::consume_all_guide(&ctx.guide);
        if msgs.is_empty() {
            queue::drain_pending_to_guide(&ctx.guide, &ctx.pending);
            msgs = queue::consume_all_guide(&ctx.guide);
        }
        if !msgs.is_empty() {
            // === 上下文压缩检查（pre-turn）===
            // 同步执行：调一次 LLM(tools=[]) 拿摘要 → mark_compaction 落库 → 重建 messages
            // 失败 log warn 跳过本次压缩，主流程继续
            run_pre_turn_compression(&ctx, &mut session, &msgs).await;

            // 取第一条消息的 params（决定本轮 model/options）
            // ReAct 多轮复用同一份 model（一个 turn 一个模型）
            // TODO: 多条 guide 消息 params 不一致时如何取——当前取第一条
            let first_params = msgs.first().map(|m| m.params.clone()).unwrap_or_default();
            // 一次性全部注入：每条变一条 user message（入队时已过管道发回显，此处只推进历史）
            queue::inject_messages(&mut session, msgs);
            turn::run_turn(&ctx, &mut session, &mut rx_interrupt, first_params).await;
        } else {
            // guide 空：等入站消息（过管道入队）/ 中断 / Plugin 通知
            tokio::select! {
                Some(inbound) = rx_inbound.recv() => {
                    // 入站 User 消息过完整管道：拦截 → 处理(入队) → 发送(回显) → 观察
                    handle_inbound_user(&ctx, inbound).await;
                    // 回循环顶部重新 consume（刚入队的消息会驱动新 turn）
                    continue;
                }
                Some(interrupt_msg) = rx_interrupt.recv() => {
                    // idle 中断：无活跃 turn，只发通知事件
                    emit_interrupt_event(&interrupt_msg.payload, &ctx.emitter, &ctx.hooks).await;
                    tracing::debug!(session_id = %ctx.emitter.session_id(), "idle 时收到中断信号");
                }
                Some(plugin_msg) = rx_plugin.recv() => {
                    // 入站 Plugin 消息过 dispatch 管道发外部（带 session_id 标签）
                    // Plugin 消息不参与 ReAct（不触发 turn）
                    handle_inbound_plugin(&ctx, plugin_msg).await;
                }
            }
        }
    }
}

/// Pre-turn 上下文压缩检查
///
/// 在主循环注入新消息前、调 LLM 前，根据上一轮真实 usage 判定要不要压缩。
/// 触发条件满足时：调一次独立 LLM（`tools=[]`）拿摘要 → `mark_compaction` 落库 →
/// 重建 `session.messages` 为 `[compaction 边界] + [tail 保留窗口]`。
///
/// 失败处理（对齐 opencode "失败保持边界" + hermes 分级）：
/// - 摘要为空 / 无可压缩内容：log warn 跳过
/// - LLM 调用失败：log warn 跳过（不进 cooldown，下次还会触发判定）
/// - 落库失败：log warn 跳过
///
/// 同步执行：task 内串行，期间不接收新消息（天然互斥，不需要锁/队列/通道）。
async fn run_pre_turn_compression(
    ctx: &SessionCtx,
    session: &mut Session,
    incoming: &[QueuedUserMessage],
) {
    // 读取上一轮真实 usage（首轮无 usage 跳过——还没跑过没法判定）
    let usage = {
        let guard = ctx.last_usage.lock().await;
        match guard.clone() {
            Some(u) => u,
            None => return,
        }
    };

    // 解析当前模型上下文长度（用 incoming 第一条消息的 model_id）
    // ponytail: 多 guide 消息 model 不一致用第一条（与 run_turn 内 "取第一条 params" 一致）
    let model_id = incoming
        .first()
        .and_then(|m| m.params.model_config.model_id.as_deref())
        .unwrap_or("");
    let context_length = fuyao_provider::get_model(model_id, &ctx.agent_paths)
        .map(|m| m.limit.context)
        .unwrap_or(ctx.compression_config.fallback_context);

    // 阈值检测（含反抖动判定）
    let trigger = {
        let state = ctx
            .compression_state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        fuyao_session::should_compress(
            usage.prompt_tokens,
            context_length,
            &ctx.compression_config,
            &state,
        )
    };
    if !trigger {
        return;
    }

    tracing::info!(
        session_id = ctx.emitter.session_id(),
        prompt_tokens = usage.prompt_tokens,
        context_length = context_length,
        model_id = model_id,
        "触发上下文压缩"
    );

    // 执行层：生成摘要（原消息原样发，前缀缓存完整命中）
    let summary = match fuyao_session::generate_summary(
        session.system_prompt.as_deref(),
        &session.messages,
        &ctx.provider,
        model_id,
        context_length,
        &ctx.compression_config,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                cause = %e,
                "摘要生成失败，跳过本次压缩"
            );
            return;
        }
    };

    // 落地层：mark_compaction + 重建 messages
    match fuyao_session::apply(
        &session.messages,
        &summary,
        ctx.emitter.session_id(),
        &ctx.compression_config,
        context_length,
        &ctx.store,
    )
    .await
    {
        Ok(new_messages) => {
            // 重建 system_prompt：build_system_prompt 纯本地拼接（不调 LLM），
            // 保证旧 system 中残留的动态内容（如"基于刚才的 X 错误继续排查"）在
            // X 已被压进摘要后不再误导模型
            let new_prompt = fuyao_prompt::build_system_prompt(&ctx.agent_paths, &ctx.agent_config);

            // 落库新 system_prompt。失败时仅 warn 跳过：compaction 边界已落库、
            // messages 已重建（压缩核心成果保住），system_prompt 内存更新照常进行——
            // 下轮请求已经会用新 prompt，DB 字段下次 update session 时会自然同步
            if let Err(e) = ctx
                .store
                .update_system_prompt(ctx.emitter.session_id(), &new_prompt)
                .await
            {
                tracing::warn!(
                    session_id = ctx.emitter.session_id(),
                    cause = %e,
                    "system_prompt 落库失败，仅更新内存"
                );
            }

            // 更新反抖动统计
            let mut state = ctx
                .compression_state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            state.record_compaction(summary.tokens_before as u32, summary.tokens_after as u32);

            // 写回内存 session（messages + system_prompt）
            session.system_prompt = Some(new_prompt);
            session.messages = new_messages;
        }
        Err(e) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                cause = %e,
                "压缩落地失败，跳过本次压缩"
            );
        }
    }
}

/// 处理入站 User 消息：过完整管道（拦截 → 处理[入队] → 发送[回显] → 观察）
///
/// process 段按 mode 入 guide / pending 队列；deliver 段发 OutputEvent::User 回显给 UI。
fn handle_inbound_user(
    ctx: &SessionCtx,
    inbound: InboundUser,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
    let guide = Arc::clone(&ctx.guide);
    let pending = Arc::clone(&ctx.pending);
    let mode = inbound.mode;
    let queued = QueuedUserMessage {
        content: inbound.content.clone(),
        params: inbound.params,
    };
    // process 回调：按 mode 入队（瞬间、不阻塞）
    let process: dispatch::ProcessFn = Box::new(move |_| {
        let guide = guide.clone();
        let pending = pending.clone();
        Box::pin(async move {
            match mode {
                UserMessageMode::Guide => guide
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(queued),
                UserMessageMode::Pending => pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(queued),
            }
        })
    });

    Box::pin(async move {
        dispatch::dispatch(
            &ctx.emitter,
            &ctx.hooks,
            OutputEvent::User(OutputUserMessage {
                base: fuyao_api::message::EventBase::default(),
                payload: UserPayload {
                    content: inbound.content,
                    mode,
                    source: UserMessageSource::User,
                },
            }),
            Some(process),
        )
        .await;
    })
}

/// 处理入站 Plugin 消息：把 input 侧 PluginMessage 转 output 侧 OutputEvent::Plugin，
/// 过完整 dispatch 管道（拦截 → 发送 → 观察）。
///
/// process 段传 None——Plugin 消息无需特殊处理（不像 User 要入队），纯通知透传。
/// 发送时 Emitter 自动盖 session_id 标签。
fn handle_inbound_plugin(
    ctx: &SessionCtx,
    plugin_msg: PluginMessage,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
    Box::pin(async move {
        let output_event = OutputEvent::Plugin(OutputPluginMessage {
            base: plugin_msg.base,
            payload: OutputPluginPayload {
                source: plugin_msg.payload.source,
                event_type: plugin_msg.payload.event_type,
                data: plugin_msg.payload.data,
                error: plugin_msg.payload.error,
                message: plugin_msg.payload.message,
            },
        });
        dispatch::dispatch(&ctx.emitter, &ctx.hooks, output_event, None).await;
    })
}
