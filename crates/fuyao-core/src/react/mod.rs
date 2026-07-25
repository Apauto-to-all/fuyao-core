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
//!   落 DB（事件级落库），回循环顶部调 LLM
//! - pending：排队队列，AI 不再调工具（最终回复）后才一次性全部倒进 guide
//!
//! 两个消费时机（详见 [`turn::run_turn`]）：
//! - 一批工具全部执行完成后、发回 AI 前：只看 guide（还在调工具，pending 不动）
//! - AI 不调用工具（最终回复，一轮 ReAct 结束）：先 pending 全倒 guide，再 guide 全消费
//!
//! 中断通道与队列分离：Interrupt 走独立 `rx_interrupt`（mpsc），
//! select! 中断点只监听它——不会误取 User/Plugin。
//!
//! 工具结果不走队列：它是 ReAct 循环内部中间产物，产生即落 DB（事件级落库）。

mod builders;
pub(crate) mod queue;
pub(crate) mod retry;
#[cfg(test)]
mod tests;
pub(crate) mod turn;

use crate::dispatch;
use crate::emit::Emitter;
use crate::engine::types::SharedQueue;
use crate::interrupt::emit_interrupt_event;
use crate::tool_registry::ToolRegistry;
use fuyao_api::UserMessageMode;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::output::InterruptMessage as OutputInterruptMessage;
use fuyao_api::message::output::UserMessage as OutputUserMessage;
use fuyao_api::message::output::{
    CompressionDeltaPayload, CompressionEndedPayload, CompressionMessage, CompressionPayload,
    CompressionReason, CompressionStartedPayload, PluginMessage as OutputPluginMessage,
};
use fuyao_api::{CompressionConfig, EventBase, Session, SessionParams};
use fuyao_hooks::SharedHooks;
use fuyao_provider::{ProviderRegistry, StreamUsage};
use fuyao_session::CompressionRuntimeState;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;

/// session 的共享依赖（引擎级共享能力的 owned 视图）
///
/// 聚合 store / providers / tools / hooks / agent_paths / emitter / guide / pending
/// 这些所有 turn 都需要的共享只读依赖 + 双队列，避免 run_turn 参数列表过长。
/// 不含可变状态（session / rx_interrupt）——那些作为独立 &mut 参数传入。
/// 由 run_session 构造一次，整个 task 期间以 `&SessionCtx` 不可变借用复用。
pub(crate) struct SessionCtx {
    pub store: Arc<fuyao_session::SessionStore>,
    /// Provider 实例注册表（按消息级 provider_id 路由）
    pub providers: Arc<ProviderRegistry>,
    pub tools: Arc<ToolRegistry>,
    /// 钩子注册表（引擎级共享，透传给本 session 的 dispatch 管道）
    pub hooks: SharedHooks,
    pub agent_paths: fuyao_api::AgentPaths,
    /// 对话级参数（共享句柄，Engine 写 / task 现读现用）
    ///
    /// 整 session 全程只有一份：`Engine::update_session_params` 经 SessionHandle 写回，
    /// 本 ctx 现读现用（跑 turn 取 model_config、压缩取 agent_config 重建 prompt）。
    /// 用 Mutex 是因为 Engine 写与 task 读跨线程——读时持锁拷一份快照即用。
    /// agent_config 创建时定死不应改（前缀缓存红线），model_config 可随时更新。
    pub session_params: Arc<Mutex<SessionParams>>,
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
    /// 该 session 的关闭信号（Engine::shutdown 时 cancel）
    ///
    /// 引擎级 shutdown_token 的 child_token：Engine::shutdown 调 root.cancel →
    /// 所有 child 同时 cancel → 本 session 主循环 select! 收到信号优雅退出。
    /// 目前只在主循环 idle select! 监听；未来若需 turn 中途响应，可在 turn.rs
    /// 的 select! 中段也加一路监听（行为同中断，但优先级更高）。
    pub shutdown_token: CancellationToken,
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
/// 关于 SessionParams 的贯穿：`SessionParams` 从 `Engine::create_session` 整体传入，
/// 不在入口拆成 `AgentConfig`——压缩重建 system_prompt 等运行时场景仍需其中字段，
/// 故整体留存进 `SessionCtx.session_params`。将来 SessionParams 加字段时，
/// 只需在 `SessionCtx` 多存一个值，中间函数签名不动。
///
/// 多 session 并发时工具共享（引擎级 `ToolRegistry`）、人格隔离（各自 SessionParams），互不干扰。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_session(
    session_id: String,
    guide: SharedQueue,
    pending: SharedQueue,
    mut rx_inbound: Receiver<OutputUserMessage>,
    mut rx_interrupt: Receiver<OutputInterruptMessage>,
    mut rx_plugin: Receiver<OutputPluginMessage>,
    shutdown_token: CancellationToken,
    mut session: Session,
    store: Arc<fuyao_session::SessionStore>,
    providers: Arc<ProviderRegistry>,
    tools: Arc<ToolRegistry>,
    hooks: SharedHooks,
    agent_paths: fuyao_api::AgentPaths,
    session_params: Arc<Mutex<SessionParams>>,
    tx_event: Sender<OutputEvent>,
) {
    tracing::info!(session_id = %session_id, "session 执行流启动");

    let ctx = SessionCtx {
        store,
        providers,
        tools,
        hooks,
        agent_paths,
        session_params,
        emitter: Emitter::new(tx_event, session_id.clone()),
        guide,
        pending,
        last_usage: Arc::new(Mutex::new(None)),
        compression_state: Arc::new(std::sync::Mutex::new(CompressionRuntimeState::default())),
        compression_config: fuyao_api::get_config().session.compression.clone(),
        shutdown_token: shutdown_token.clone(),
    };

    // 主循环：从 guide 全取消息 → 注入 → 跑一轮 ReAct；guide 空 → 等待入站/中断/shutdown
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
            // 同步执行：调一次 LLM(tools=[]) 拿摘要 → mark_compaction 落库 → 复制 keep_recent 为新 seq
            // 失败 log warn 跳过本次压缩，主流程继续
            run_pre_turn_compression(&ctx, &mut session).await;

            // shutdown 检查（pre-turn 后）：避免压缩后又开新 turn
            // shutdown_token 在 run_pre_turn_compression 期间被 cancel 的情况下，
            // 这里 break 让 session 优雅退出（保护刚压缩完的状态不被新 turn 截断）
            if ctx.shutdown_token.is_cancelled() {
                tracing::info!(
                    session_id = %ctx.emitter.session_id(),
                    "session 收到 shutdown 信号，正在落库退出"
                );
                let _ = ctx.store.update(&session).await;
                break;
            }

            // 取本轮模型配置：从 session 的 SessionParams 现读快照（整 session 共享一份，
            // Engine::update_session_params 写回，这里读最新）。ReAct 多轮复用同一份模型。
            let model_config = {
                let p = ctx.session_params.lock().await;
                p.model_config.clone()
            };
            // 一次性全部注入：每条经 emit_to_history（拦截 → insert_message 落 DB → 发送 → 观察）
            queue::inject_messages(&ctx, &mut session, msgs).await;
            turn::run_turn(&ctx, &mut session, &mut rx_interrupt, model_config).await;
        } else {
            // guide 空：等入站消息（过管道入队）/ 中断 / Plugin 通知 / shutdown
            tokio::select! {
                biased;
                // shutdown 优先胜出（即使有消息积压也先退出）
                _ = ctx.shutdown_token.cancelled() => {
                    tracing::info!(
                        session_id = %ctx.emitter.session_id(),
                        "session 收到 shutdown 信号，正在落库退出"
                    );
                    // 落库保护 in-flight 状态（失败仅 warn，不阻塞关闭）
                    if let Err(e) = ctx.store.update(&session).await {
                        tracing::warn!(
                            session_id = %ctx.emitter.session_id(),
                            cause = %e,
                            "shutdown 落库失败，session 状态可能丢失最近一条未持久化的消息"
                        );
                    }
                    break;
                }
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
/// 复制 keep_recent 为新 seq（下次 `load_visible_messages` 自然看到 compaction 边界 + keep_recent）。
///
/// 失败处理（失败保持边界 + 错误分级）：
/// - 摘要为空 / 无可压缩内容：log warn 跳过
/// - LLM 调用失败：log warn 跳过（不进 cooldown，下次还会触发判定）
/// - 落库失败：log warn 跳过
///
/// 同步执行：task 内串行，期间不接收新消息（天然互斥，不需要锁/队列/通道）。
async fn run_pre_turn_compression(ctx: &SessionCtx, session: &mut Session) {
    // 读取上一轮真实 usage（首轮无 usage 跳过——还没跑过没法判定）
    let usage = {
        let guard = ctx.last_usage.lock().await;
        match guard.clone() {
            Some(u) => u,
            None => return,
        }
    };

    // 解析本轮主模型的 model_id 和 Provider 实例
    //
    // **前缀缓存红线**：压缩必须用主对话这一轮的同一个 Provider/endpoint，
    // 否则原样发的请求会因为 endpoint 切换导致前缀缓存失效。
    // model_id 解析顺序与 turn.rs::resolve_model 一致：
    //   1. SessionParams.model_config.model_id = Some(...) → 用它（整 session 共享一份，现读）
    //   2. None → 读 [models.default] 兜底
    //   3. 都没有 → 无法确定主模型，跳过本次压缩（warn 记录原因）
    let explicit_id: Option<String> = {
        let p = ctx.session_params.lock().await;
        p.model_config.model_id.as_deref().map(|id| id.to_string())
    };
    let model_id: String = match explicit_id {
        Some(id) => id,
        None => match fuyao_api::get_config()
            .models
            .default
            .as_ref()
            .map(|r| r.model.clone())
            .filter(|s| !s.is_empty())
        {
            Some(id) => id,
            None => {
                tracing::warn!(
                    session_id = ctx.emitter.session_id(),
                    "压缩跳过：本轮主模型未指定且未配置 [models.default]"
                );
                return;
            }
        },
    };

    // 拆 provider_id → 从 registry 取 Provider 实例（与主对话 stream_chat 同一个）
    let provider_id = match model_id.split_once('/') {
        Some((p, _)) if !p.is_empty() => p.to_lowercase(),
        _ => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                model_id = %model_id,
                "压缩跳过：model_id 格式错误"
            );
            return;
        }
    };
    let provider = match ctx.providers.get(&provider_id) {
        Some(p) => p,
        None => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                provider_id = %provider_id,
                "压缩跳过：Provider 实例未注册"
            );
            return;
        }
    };

    let context_length = fuyao_provider::get_model(&model_id, &ctx.agent_paths)
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

    // 发 Compression Started 事件：调摘要 LLM 之前，让前端显示"压缩中..."状态
    dispatch::dispatch(
        &ctx.emitter,
        &ctx.hooks,
        OutputEvent::Compression(CompressionMessage {
            base: EventBase::default(),
            payload: CompressionPayload::Started(CompressionStartedPayload {
                reason: CompressionReason::Auto,
                prompt_tokens: usage.prompt_tokens,
                context_length,
            }),
        }),
        None,
    )
    .await;

    // 从 DB 加载当前可见消息（事件级落库模式下消息不在内存）
    let visible_messages = match ctx.store.load_visible_messages(session.id.as_str()).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                cause = %e,
                "压缩前加载可见消息失败，跳过本次压缩"
            );
            return;
        }
    };

    // 执行层：生成摘要（原消息原样发，前缀缓存完整命中）
    //
    // 流式增量通过 channel 转发到并发的 Delta 事件发送任务：
    // - callback 是同步 FnMut，无法 await dispatch，所以用 try_send 推到 channel
    // - select! 并发：generate_summary 与 Delta 消费者同时跑，每收到一条就发 Compression Delta
    // - channel 满了 try_send 失败就丢（Delta 本就是 live-only 增量，丢得起）
    let (delta_tx, mut delta_rx) =
        tokio::sync::mpsc::channel::<(Option<String>, Option<String>)>(32);

    // callback 用 let 绑定避免临时值生命周期问题（future 会借用它）
    let mut on_delta = |content: Option<&str>, reasoning: Option<&str>| {
        let _ = delta_tx.try_send((content.map(String::from), reasoning.map(String::from)));
    };

    // Delta 消费者：循环从 channel 取 delta 发事件，delta_tx drop 后 recv 返回 None 退出
    let mut delta_consumer = Box::pin(async {
        while let Some((content, reasoning)) = delta_rx.recv().await {
            dispatch::dispatch(
                &ctx.emitter,
                &ctx.hooks,
                OutputEvent::Compression(CompressionMessage {
                    base: EventBase::default(),
                    payload: CompressionPayload::Delta(CompressionDeltaPayload {
                        content,
                        reasoning,
                    }),
                }),
                None,
            )
            .await;
        }
    });

    // summary 与 Delta 消费者并发跑；summary 完成后 on_delta（含 delta_tx）drop，
    // Delta 消费者 recv 返回 None 自然退出
    let summary = tokio::select! {
        biased;
        s = fuyao_session::generate_summary(
            session.system_prompt.as_deref(),
            &visible_messages,
            &provider,
            &model_id,
            context_length,
            &ctx.compression_config,
            &mut on_delta,
        ) => {
            match s {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        session_id = ctx.emitter.session_id(),
                        cause = %e,
                        "摘要生成失败，跳过本次压缩"
                    );
                    return;
                }
            }
        }
        _ = &mut delta_consumer => {
            unreachable!("Delta 消费者先于 summary 结束")
        }
    };
    // 等 Delta 消费者把剩余积压推完（summary 完成后 on_delta/delta_tx drop，recv 返回 None 退出）
    let _ = (&mut delta_consumer).await;

    // 落地层：mark_compaction + 复制 keep_recent 为新 seq
    match fuyao_session::apply(
        &visible_messages,
        &summary,
        ctx.emitter.session_id(),
        &ctx.compression_config,
        context_length,
        ctx.store.as_ref(),
    )
    .await
    {
        Ok(new_seq) => {
            // 重建 system_prompt：build_system_prompt 纯本地拼接（不调 LLM），
            // 保证旧 system 中残留的动态内容（如"基于刚才的 X 错误继续排查"）在
            // X 已被压进摘要后不再误导模型
            // agent_config 创建时定死不应变（前缀缓存红线），此处锁取快照即用
            let agent_config = {
                let p = ctx.session_params.lock().await;
                p.agent_config.clone()
            };
            let new_prompt = fuyao_prompt::build_system_prompt(&ctx.agent_paths, &agent_config);

            // 落库新 system_prompt。失败时仅 warn 跳过：compaction 边界已落库、
            // keep_recent 已复制，system_prompt 内存更新照常进行——下轮请求已经会用新 prompt，
            // DB 字段下次 update session 时会自然同步
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

            // 更新反抖动统计（用块 scope 限定 MutexGuard 生命周期，避免跨 await 持锁）
            {
                let mut state = ctx
                    .compression_state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                state.record_compaction(summary.tokens_before as u32, summary.tokens_after as u32);
            }

            // 写回内存 session 的 system_prompt（messages 不在内存，无需重建）
            session.system_prompt = Some(new_prompt);

            // 发 Compression Ended 事件：apply 落库成功后，让前端移除"压缩中"状态、展示统计
            dispatch::dispatch(
                &ctx.emitter,
                &ctx.hooks,
                OutputEvent::Compression(CompressionMessage {
                    base: EventBase::default(),
                    payload: CompressionPayload::Ended(CompressionEndedPayload {
                        reason: CompressionReason::Auto,
                        content: summary.content.clone(),
                        tokens_before: summary.tokens_before as u32,
                        tokens_after: summary.tokens_after as u32,
                        new_seq,
                    }),
                }),
                None,
            )
            .await;
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

/// 处理入站 User 消息：**纯入队**（无任何 side effect）
///
/// 入队只负责按 mode 分流到 guide / pending 队列，**不做**拦截、不发事件、不触发钩子。
/// 所有处理（拦截 / push / 发送 / 观察）推迟到 `inject_messages` 消费时统一过
/// `emit_to_history` 管道——与 assistant / tool_result 走完全相同的路径。
///
/// 这样保证 user 消息的拦截/push/发送三个时机**对齐**（都在消费时刻），
/// 修复"以输入消息为核心组织"导致的三时机错位（拦截提前、发送提前、push 延迟）。
async fn handle_inbound_user(ctx: &SessionCtx, inbound: OutputUserMessage) {
    let mode = inbound.payload.mode;
    match mode {
        UserMessageMode::Guide => ctx
            .guide
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(inbound),
        UserMessageMode::Pending => ctx
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(inbound),
    }
}

/// 处理入站 Plugin 消息：通道承载的就是 output 侧 PluginMessage，
/// 直接包成 `OutputEvent::Plugin` 过完整 dispatch 管道（拦截 → 发送 → 观察）。
///
/// process 段传 None——Plugin 消息无需特殊处理（不像 User 要入队），纯通知透传。
/// 发送时 Emitter 自动盖 session_id 标签。
fn handle_inbound_plugin(
    ctx: &SessionCtx,
    plugin_msg: OutputPluginMessage,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
    Box::pin(async move {
        dispatch::dispatch(
            &ctx.emitter,
            &ctx.hooks,
            OutputEvent::Plugin(plugin_msg),
            None,
        )
        .await;
    })
}
