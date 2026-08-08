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
mod normalize;
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
use fuyao_api::message::input::UserMessageSource;
use fuyao_api::message::output::InterruptMessage as OutputInterruptMessage;
use fuyao_api::message::output::UserMessage as OutputUserMessage;
use fuyao_api::message::output::{
    CompressionDeltaPayload, CompressionEndedPayload, CompressionMessage, CompressionPayload,
    CompressionReason, CompressionStartedPayload, PluginMessage as OutputPluginMessage,
    RollbackMessage, RollbackPayload, UserPayload,
};
use fuyao_api::{AgentDefinition, CompressionConfig, ControlCommand, EventBase, SessionParams};
use fuyao_hooks::SharedHooks;
use fuyao_provider::{ProviderRegistry, StreamUsage};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc::{Receiver, UnboundedSender};
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
    /// 当前 session 的 Agent 定义（创建时加载定型，整 session 不可变）
    ///
    /// 由 `assemble_session` 经 `fuyao_prompt::resolve_definition` 加载（含 mode 校验 +
    /// 回退）。per-session 持有完整定义：
    /// - `.tools` 供 `resolve_model` 做 per-session 工具可见性过滤（定义层工具配置，
    ///   与全局 `[tools.enabled]` 取交集；工具集改变会冲掉前缀缓存，故创建时定死）
    /// - 后续 definition 字段扩展（如权限、资源限制）可直接复用本字段，无需再加容器
    pub definition: AgentDefinition,
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
    /// 压缩配置（从全局 config 读取，启动时定死）
    pub compression_config: CompressionConfig,
    /// 该 session 的关闭信号（Engine::shutdown 时 cancel）
    ///
    /// 引擎级 shutdown_token 的 child_token：Engine::shutdown 调 root.cancel →
    /// 所有 child 同时 cancel → 本 session 主循环 select! 收到信号优雅退出。
    /// 目前只在主循环 idle select! 监听；未来若需 turn 中途响应，可在 turn.rs
    /// 的 select! 中段也加一路监听（行为同中断，但优先级更高）。
    pub shutdown_token: CancellationToken,
    /// 引擎派生子 session 的能力弱引用（注入工具 ctx，子代理类工具用）
    ///
    /// 引擎级共享，跨 session 不变；放 SessionCtx 让 turn.rs 调 execute_tools 时
    /// 便于透传到 [`crate::tool_exec::execute_single`] 构造的 ToolCallContext。
    /// `Option` 让测试场景可传 `None`（避免 `Weak::<dyn Trait>::new()` 的 Sized 限制）。
    pub subagent_ops: Option<std::sync::Weak<dyn fuyao_api::SubagentOps>>,
    /// 本 session 是否为子任务 session（`parent_session_id` 非空）
    ///
    /// 创建时定死、整 session 不变（来自 DB session 行的 `parent_session_id`）。
    /// 用途：`resolve_model` 据此过滤子 session 不可见的工具（递归防护）、
    /// 标题生成与压缩的子 session 豁免判定。
    ///
    /// 取代旧的"内存 `session.parent_session_id` 现读"——DB 唯一数据源后，
    /// session 不再常驻内存，此标记提升为 ctx 的不可变字段。
    pub is_child: bool,
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
    mut rx_control: Receiver<ControlCommand>,
    shutdown_token: CancellationToken,
    is_child: bool,
    store: Arc<fuyao_session::SessionStore>,
    providers: Arc<ProviderRegistry>,
    tools: Arc<ToolRegistry>,
    hooks: SharedHooks,
    agent_paths: fuyao_api::AgentPaths,
    definition: AgentDefinition,
    session_params: Arc<Mutex<SessionParams>>,
    tx_event: UnboundedSender<OutputEvent>,
    subagent_ops: Option<std::sync::Weak<dyn fuyao_api::SubagentOps>>,
) {
    tracing::info!(session_id = %session_id, is_child = is_child, "session 执行流启动");

    let ctx = SessionCtx {
        is_child,
        store,
        providers,
        tools,
        hooks,
        agent_paths,
        definition,
        session_params,
        emitter: Emitter::new(tx_event, session_id.clone()),
        guide,
        pending,
        last_usage: Arc::new(Mutex::new(None)),
        compression_config: fuyao_api::get_config().session.compression.clone(),
        shutdown_token: shutdown_token.clone(),
        subagent_ops,
    };

    // Plugin 转发独立 task：通知一到就转发，不阻塞在主循环的 turn 上
    //
    // Plugin 消息是纯通知：仅过 dispatch 管道转发（拦截 → 发送 → 观察），不碰 session
    // 可变状态、不碰 guide/pending 队列、不参与 ReAct，故可与活跃 turn 安全并发。
    // 修复前 Plugin 仅在主循环 idle select! 消费，turn 运行期间（pre-turn 压缩 +
    // 注入 + LLM 流式 + 工具执行，可能很久）通知堆在通道里，延迟整轮才转发。
    //
    // 退出条件（双重，任一满足即退）：① shutdown_token 取消（Engine::shutdown 联动）；
    // ② 所有 tx_plugin drop 致 recv 返 None（SessionHandle drop 时其 tx_plugin 随之 drop）。
    // 无需追踪 forwarder 的 JoinHandle——靠 tx drop + shutdown_token 自然收尾，不过度设计。
    {
        let emitter = ctx.emitter.clone();
        let hooks = ctx.hooks.clone();
        let shutdown_token = ctx.shutdown_token.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    // shutdown 优先：取消即退，不等通道排空
                    _ = shutdown_token.cancelled() => break,
                    plugin_msg = rx_plugin.recv() => match plugin_msg {
                        Some(msg) => {
                            dispatch::dispatch(
                                &emitter,
                                &hooks,
                                OutputEvent::Plugin(msg),
                                None,
                            )
                            .await;
                        }
                        // 所有 tx_plugin 已 drop（session 结束）→ 退
                        None => break,
                    },
                }
            }
        });
    }

    // 主循环：从 guide 全取消息 → 注入 → 跑 ReAct（自包含循环）。
    //
    // 消费许可由 `outcome` 表达：只有上次 turn `Completed`（双队列跑空）才允许 consume guide。
    // 任何非 Completed 退出（命令停 / 中断 / 失败）→ outcome 保持非 Completed → 下次 loop 顶部
    // 跳过 consume，guide/pending 剩余原样保留，落 select! 等待。
    //
    // 恢复消费：select! 的 inbound 分支收到新用户消息时把 outcome 重置为 Completed——
    // 新消息入队 = 新意图，回顶部 consume 把「旧剩余 + 新消息」一起跑（忠实消费，引擎不清队列）。
    let mut outcome = turn::TurnOutcome::Completed;
    loop {
        // === 控制通道消费（turn 边界）===
        // 每次循环顶部 try_recv 排空控制通道：处理 turn 运行期间到达的 B 类命令（手动压缩等），
        // 在 guide 检查前于 turn 边界执行（不抢占在途 turn，等当前 turn 跑完下轮顶部才处理）。
        // 忠实执行：每条命令各跑一次，不做同批次去重 / 合并——引擎是忠实执行器，不解释意图
        //（去重是上层职责，见 AGENTS.md「引擎是忠实执行器」）。
        while let Ok(cmd) = rx_control.try_recv() {
            handle_control(&ctx, cmd).await;
        }
        // 消费许可：上次 turn 非 Completed（被命令停 / 中断 / 失败）→ 跳过 consume，
        // guide/pending 剩余原样保留，直接落 select! 等待用户新消息恢复
        if matches!(outcome, turn::TurnOutcome::Completed) {
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
                // 同步执行：调一次 LLM(tools=[]) 拿摘要 → mark_compaction 落库
                // 失败 log warn 跳过本次压缩，主流程继续
                run_pre_turn_compression(&ctx).await;

                // shutdown 检查（pre-turn 后）：避免压缩后又开新 turn
                // shutdown_token 在 run_pre_turn_compression 期间被 cancel 的情况下，
                // 这里 break 让 session 优雅退出（保护刚压缩完的状态不被新 turn 截断）
                if ctx.shutdown_token.is_cancelled() {
                    tracing::info!(
                        session_id = %ctx.emitter.session_id(),
                        "session 收到 shutdown 信号，退出"
                    );
                    break;
                }

                // 取本轮模型配置：从 session 的 SessionParams 现读快照（整 session 共享一份，
                // Engine::update_session_params 写回，这里读最新）。ReAct 多轮复用同一份模型。
                let model_config = {
                    let p = ctx.session_params.lock().await;
                    p.model_config.clone()
                };
                // 一次性全部注入：每条经 emit_to_history（拦截 → insert_message 落 DB → 发送 → 观察）
                queue::inject_messages(&ctx, msgs).await;
                // 首轮 user 消息落库后立即触发标题生成（fire-and-forget，不等 AI 回复）：
                // 在 run_turn 之前判定，解决旧逻辑「等 AI 整轮回复完成才生成」的延迟硬伤。
                // 内部按 user_count==1 判首轮，仅首轮通过，后续轮次天然跳过。
                turn::maybe_spawn_title_generation(&ctx, ctx.is_child).await;
                // run_turn 自包含跑完整个队列直到空、或被控制命令/中断打断 → return TurnOutcome。
                // outcome 决定下一轮 loop 顶部的消费许可：非 Completed 则跳过 consume 等恢复。
                outcome =
                    turn::run_turn(&ctx, &mut rx_interrupt, &mut rx_control, model_config).await;
            }
        }
        // === 等待（无条件）===
        // 三类情况都进这里：
        // ① guide 空（Completed 且无消息）② run_turn 非 Completed return（队列剩余被保留）
        // ③ 消费被跳过（outcome 非 Completed）。
        // 停止消费：非 Completed 时 guide 剩余不跑，落这里等。
        // 恢复消费：inbound 收到新用户消息 → 重置 outcome=Completed → 回顶部 consume，
        // 旧剩余 + 新消息一起跑（忠实消费，不清队列）。
        // （Plugin 通知由 run_session 顶部 spawn 的独立 forwarder task 并发转发，
        // 不在主循环消费——避免 turn 运行期间通知被阻塞延迟整轮）
        tokio::select! {
            biased;
            // shutdown 优先胜出（即使有消息积压也先退出）
            _ = ctx.shutdown_token.cancelled() => {
                tracing::info!(
                    session_id = %ctx.emitter.session_id(),
                    "session 收到 shutdown 信号，退出"
                );
                break;
            }
            Some(inbound) = rx_inbound.recv() => {
                // 入站 User 消息过完整管道：拦截 → 处理(入队) → 发送(回显) → 观察。
                // 重置消费许可：新消息 = 新意图，回顶部 consume（旧剩余 + 新消息一起跑）
                outcome = turn::TurnOutcome::Completed;
                handle_inbound_user(&ctx, inbound).await;
            }
            Some(interrupt_msg) = rx_interrupt.recv() => {
                // idle 中断：无活跃 turn，只发通知事件
                emit_interrupt_event(&interrupt_msg.payload, &ctx.emitter, &ctx.hooks).await;
                tracing::debug!(session_id = ctx.emitter.session_id(), "idle 时收到中断信号");
            }
            Some(cmd) = rx_control.recv() => {
                // idle 时控制命令到达：执行（同 task 串行消费，天然互斥）。
                // select! 结束后回顶部 drain 取其余
                handle_control(&ctx, cmd).await;
            }
        }
    }
}

/// 压缩执行依赖的解析结果（model_id / 思考配置 / provider / 上下文长度）
///
/// 子代理 session 的压缩豁免判定（可配置，`[session.compression] skip_child` 默认 true）
///
/// 子代理以 Fresh 模式派生、只回传最终回复文本给父 Agent，自身完整历史留在子 session 内。
/// 若子代理中途压缩，早期工具证据会被摘要替代，最终回复失真并作为 tool_result 传导给父 Agent
/// 的决策；且压缩需额外调一次摘要 LLM，对一次性子代理在成本与质量上均不划算。
/// 仅自动触发路径（[`run_pre_turn_compression`]）使用此豁免——引擎替用户挡不划算的压缩；
/// 手动触发（[`run_manual_compression`]）不豁免：用户显式要对子会话压缩是用户的选择，照做。
fn compression_exempt(ctx: &SessionCtx) -> bool {
    ctx.is_child && ctx.compression_config.skip_child
}

/// 解析压缩执行所需的模型信息
///
/// **前缀缓存红线**：压缩必须用主对话这一轮的同一个 Provider/endpoint，否则原样发的请求
/// 会因 endpoint 切换导致前缀缓存失效。
///
/// model_id / thinking / context_length 解析统一走 [`builders::resolve_model`]（与主对话
/// `run_turn` 同一份逻辑，避免散算漂移）；provider 实例是压缩路径专属步骤——
/// [`builders::resolve_model`] 故意不查 registry（保持纯构造边界），故在此单独取。
///
/// 注：写回逻辑（turn.rs）已在首轮后把 model_id + thinking 物化进 session_params，
/// 正常运行期这里读到的都是 Some。None→default 分支仅首轮前 / 未物化时兜底。
async fn resolve_compression_model(
    ctx: &SessionCtx,
) -> Option<(
    builders::ResolvedModel,
    std::sync::Arc<dyn fuyao_provider::Provider>,
)> {
    let model_config = ctx.session_params.lock().await.model_config.clone();
    let resolved = builders::resolve_model(
        &model_config,
        &ctx.tools,
        ctx.is_child,
        &ctx.definition.tools,
        &ctx.agent_paths,
        ctx.compression_config.fallback_context,
    )
    .map_err(|msg| {
        tracing::warn!(
            session_id = ctx.emitter.session_id(),
            cause = %msg,
            "压缩跳过：模型解析失败（model_id 无效或未配置 [models.default]）"
        );
        msg
    })
    .ok()?;

    let provider = ctx.providers.get(&resolved.provider_id).or_else(|| {
        tracing::warn!(
            session_id = ctx.emitter.session_id(),
            provider_id = %resolved.provider_id,
            "压缩跳过：Provider 实例未注册"
        );
        None
    })?;

    Some((resolved, provider))
}

/// Pre-turn 自动上下文压缩检查（「该不该压」的阈值门）
///
/// 在主循环注入新消息前、调 LLM 前，根据上一轮真实 usage 判定要不要压缩。
/// 触发条件满足时调 [`run_compression`]（reason = auto）。手动触发见 [`run_manual_compression`]。
async fn run_pre_turn_compression(ctx: &SessionCtx) {
    if compression_exempt(ctx) {
        return;
    }

    // 读取上一轮真实 usage（首轮无 usage 跳过——还没跑过没法判定）
    let usage = match ctx.last_usage.lock().await.clone() {
        Some(u) => u,
        None => return,
    };

    let (model, provider) = match resolve_compression_model(ctx).await {
        Some(v) => v,
        None => return,
    };

    // 阈值检测：prompt_tokens >= threshold × (context_length - summary_max_tokens)
    let trigger = fuyao_session::should_compress(
        usage.prompt_tokens,
        model.context_length,
        &ctx.compression_config,
    );
    if !trigger {
        return;
    }

    run_compression(
        ctx,
        CompressionReason::Auto,
        usage.prompt_tokens,
        &model,
        &provider,
    )
    .await;
}

/// 手动触发上下文压缩（控制通道 Compress 命令的处理）
///
/// 与自动压缩（[`run_pre_turn_compression`]）共用 [`run_compression`] 执行流程，区别有二：
/// ① 跳过阈值检测与反抖动（用户意图优先，不判「该不该压」）；
/// ② 不适用子会话豁免——用户显式要对子会话压缩是用户的选择，引擎照做，
///    子代理失真风险由用户自担（自动压缩替用户挡不划算的压缩，手动不挡）。
/// 触发原因标记为 manual。
async fn run_manual_compression(ctx: &SessionCtx) {
    let (model, provider) = match resolve_compression_model(ctx).await {
        Some(v) => v,
        None => return,
    };

    // prompt_tokens：取上一轮真实 usage（首轮前无 usage 则 0——仅用于 Started 事件展示）
    let prompt_tokens = ctx
        .last_usage
        .lock()
        .await
        .as_ref()
        .map(|u| u.prompt_tokens)
        .unwrap_or(0);

    run_compression(
        ctx,
        CompressionReason::Manual,
        prompt_tokens,
        &model,
        &provider,
    )
    .await;
}

/// 执行一次上下文压缩（「怎么压」的执行体）
///
/// 自动 / 手动两条触发路径共用本函数：
/// - 自动（[`run_pre_turn_compression`]）：先过阈值门 + 反抖动，命中才调（reason = auto）
/// - 手动（[`run_manual_compression`]）：跳过阈值门直接调（reason = manual，用户意图优先）
///
/// 流程：发 Started → 调摘要 LLM（流式 Delta 并发转发）→ apply 落库 → 发 Ended。
/// 失败处理（失败保持边界 + 错误分级）：
/// - 摘要为空 / 无可压缩内容：log warn 跳过
/// - LLM 调用失败：log warn 跳过（不进 cooldown，下次还会触发判定）
/// - 落库失败：log warn 跳过
///
/// 同步执行：task 内串行，期间不接收新消息（天然互斥，不需要锁/队列/通道）。
async fn run_compression(
    ctx: &SessionCtx,
    reason: CompressionReason,
    prompt_tokens: u32,
    model: &builders::ResolvedModel,
    provider: &std::sync::Arc<dyn fuyao_provider::Provider>,
) {
    tracing::info!(
        session_id = ctx.emitter.session_id(),
        reason = ?reason,
        prompt_tokens = prompt_tokens,
        context_length = model.context_length,
        model_id = %model.model_id,
        "触发上下文压缩"
    );

    // 发 Compression Started 事件：调摘要 LLM 之前，让前端显示"压缩中..."状态
    dispatch::dispatch(
        &ctx.emitter,
        &ctx.hooks,
        OutputEvent::Compression(CompressionMessage {
            base: EventBase::default(),
            payload: CompressionPayload::Started(CompressionStartedPayload {
                reason,
                prompt_tokens,
                context_length: model.context_length,
            }),
        }),
        None,
    )
    .await;

    // 从 DB 加载可见窗口：与主对话同口径（effective_keep_tokens 按 context_length 算），
    // 前缀缓存可复用。generate_summary 内部不切窗，把传入 messages 全量发给 LLM
    let keep_tokens = ctx
        .compression_config
        .effective_keep_tokens(model.context_length);
    let visible_messages = match ctx
        .store
        .load_visible_messages(ctx.emitter.session_id(), keep_tokens)
        .await
    {
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

    // 构造压缩用 options：复用 session 思考配置（tools 由 generate_summary 内部强制清空）
    let compression_options = fuyao_provider::StreamOptions {
        thinking_type: model.options.thinking_type.clone(),
        reasoning_effort: model.options.reasoning_effort.clone(),
        ..fuyao_provider::StreamOptions::default()
    };

    // callback 用 let 绑定避免临时值生命周期问题（future 会借用它）
    // move：让闭包持有 delta_tx，select! 后 drop(on_delta) 即释放 delta_tx，
    // 使 delta_consumer 的 recv 返回 None 退出（否则 delta_tx 留在作用域致死锁）
    let mut on_delta = move |content: Option<&str>, reasoning: Option<&str>| {
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

    // system_prompt 从 DB 现读（压缩重建后已落库，这里读到的是最新值）
    let system_prompt: Option<String> = match ctx.store.get(ctx.emitter.session_id()).await {
        Ok(Some(s)) => s.system_prompt,
        Ok(None) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                "压缩时 session 行不存在，system_prompt 取空"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                cause = %e,
                "压缩时读 system_prompt 失败，按空继续"
            );
            None
        }
    };

    // summary 与 Delta 消费者并发跑；summary 完成后 on_delta（含 delta_tx）drop，
    // Delta 消费者 recv 返回 None 自然退出
    let summary = tokio::select! {
        biased;
        s = fuyao_session::generate_summary(
            system_prompt.as_deref(),
            &visible_messages,
            provider,
            &model.model_id,
            compression_options,
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
    // drop on_delta：释放其持有的 delta_tx → delta_consumer 的 recv 返回 None 自然退出
    drop(on_delta);
    // 等 Delta 消费者把剩余积压推完
    let _ = (&mut delta_consumer).await;

    // 落地层：写 compaction 边界消息（不复制 keep_recent——可见窗口在读取侧动态拼接）
    match fuyao_session::apply(&summary, ctx.emitter.session_id(), ctx.store.as_ref()).await {
        Ok(new_seq) => {
            // 重建 system_prompt：build_system_prompt 纯本地拼接（不调 LLM），
            // 保证旧 system 中残留的动态内容（如"基于刚才的 X 错误继续排查"）在
            // X 已被压进摘要后不再误导模型
            // definition 创建时定死不应变（前缀缓存红线），复用 ctx.definition 零加载
            // 用途按 parent_session_id 推断：子 session（子代理）用 Subagent 校验，
            // 主 session / fork 用 Primary。与创建时的用途保持一致。
            let usage = if ctx.is_child {
                fuyao_prompt::PromptUsage::Subagent
            } else {
                fuyao_prompt::PromptUsage::Primary
            };
            let new_prompt =
                fuyao_prompt::build_system_prompt(&ctx.agent_paths, &ctx.definition, usage);

            // 落库新 system_prompt（单字段 UPDATE，DB 唯一数据源）。
            // 失败时仅 warn 跳过：compaction 边界已落库，下轮请求 build_chat_request
            // 从 DB 读到的仍是旧 prompt——影响有限，不阻塞压缩流程
            if let Err(e) = ctx
                .store
                .update_system_prompt(ctx.emitter.session_id(), &new_prompt)
                .await
            {
                tracing::warn!(
                    session_id = ctx.emitter.session_id(),
                    cause = %e,
                    "system_prompt 落库失败，下轮请求仍用旧 prompt"
                );
            }

            // 发 Compression Ended 事件：apply 落库成功后，让前端移除"压缩中"状态、展示摘要
            dispatch::dispatch(
                &ctx.emitter,
                &ctx.hooks,
                OutputEvent::Compression(CompressionMessage {
                    base: EventBase::default(),
                    payload: CompressionPayload::Ended(CompressionEndedPayload {
                        reason,
                        content: summary.content.clone(),
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

/// 处理一条控制命令（turn 边界 / ReAct 间隙执行）
///
/// 控制通道载荷在此分发：每个 [`ControlCommand`] 变体对应一个执行体。
/// 命令是否要求 turn 退出由命令自身的 [`ControlCommand::turn_directive`] 表达，
/// 本函数只负责执行，不决策。
///
/// 三处调用（同一 task 串行消费，天然互斥）：
/// - 主循环顶部 drain 与 idle select! 臂（turn 边界）
/// - run_turn 内 ReAct loop 顶部间隙检查点
async fn handle_control(ctx: &SessionCtx, cmd: ControlCommand) {
    match cmd {
        ControlCommand::Compress => run_manual_compression(ctx).await,
        ControlCommand::Rollback { target_seq } => run_rollback(ctx, target_seq).await,
    }
}

/// 执行对话回退（控制通道 Rollback 命令的处理）
///
/// 与手动压缩在控制通道里地位对等：都是 task 在 turn 边界自执行的 DB 写命令。
/// 复用 store 层 [`fuyao_session::SessionStore::rollback_to`] 的单事务原子执行体
/// （删目标 seq 之后的所有消息 + 重算 count 类字段 + 局部 UPDATE sessions）。
///
/// 三步：
/// 1. 调 `rollback_to`（内部已校验目标 role/kind，非法目标事务回滚、DB 不变；
///    重算的 count 类字段已在事务内局部 UPDATE 写回 sessions 表——DB 唯一数据源，
///    无需内存刷新）
/// 2. 投影：把领域结果 `RollbackResult` 转成 wire 载荷 `RollbackPayload`
///    （目标消息本体 `Message` → `UserPayload`，按「回退后将作为新 guide 重新发送」
///    补 mode=Guide / source=User——这是应用语义，属 react 层职责，不入 store）
/// 3. 发 `OutputEvent::Rollback` 事件（经 dispatch 管道：拦截 → 发送 → 观察），
///    前端据此显示「已回退 N 条」通知 + 把目标用户消息填输入框
///
/// 失败处理：`rollback_to` 返回错误时（目标不存在 / 非法目标 / session 不存在），
/// 发 `OutputEvent::Error` 让前端感知，不 panic、不影响 task 后续运行（turn 边界语义：
/// 回退失败等价于没回退，task 继续按原状态跑）。
async fn run_rollback(ctx: &SessionCtx, target_seq: i64) {
    let result = match ctx
        .store
        .rollback_to(ctx.emitter.session_id(), target_seq)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                target_seq = target_seq,
                cause = %e,
                "对话回退失败"
            );
            dispatch::dispatch(
                &ctx.emitter,
                &ctx.hooks,
                OutputEvent::Error(fuyao_api::message::output::ErrorMessage {
                    base: EventBase::default(),
                    payload: fuyao_api::message::output::ErrorPayload {
                        message: format!("对话回退失败：{e}"),
                        recoverable: true,
                    },
                }),
                None,
            )
            .await;
            return;
        }
    };

    // rollback_to 事务内已把重算的 count 类字段局部 UPDATE 写回 sessions 表，
    // DB 是唯一数据源——无需内存刷新（下轮 build_chat_request 等读路径均从 DB 取）。

    tracing::info!(
        session_id = ctx.emitter.session_id(),
        target_seq = result.target_seq,
        deleted_total = result.deleted_total,
        message_count = result.message_count,
        "对话回退完成"
    );

    // 投影：领域结果 → output wire 载荷
    let payload = RollbackPayload {
        target_seq: result.target_seq,
        deleted_count: result.deleted_count,
        deleted_total: result.deleted_total,
        // 目标 user 消息 → UserPayload（填输入框）：取 content/images，
        // 按「回退后将作为新 guide 重新发送」补 mode/source——原消息的 mode/source 语义不再适用
        target_message: result.target_message.as_ref().map(|m| UserPayload {
            content: m.content.clone().unwrap_or_default(),
            images: m.images.clone(),
            mode: UserMessageMode::Guide,
            source: UserMessageSource::User,
        }),
        message_count: result.message_count,
        tool_call_count: result.tool_call_count,
        last_compacted_seq: result.last_compacted_seq,
        compression_count: result.compression_count,
    };

    // 发 Rollback 事件：前端据此显示「已回退 N 条」通知 + 把目标用户消息填输入框
    dispatch::dispatch(
        &ctx.emitter,
        &ctx.hooks,
        OutputEvent::Rollback(RollbackMessage {
            base: EventBase::default(),
            payload,
        }),
        None,
    )
    .await;
}
