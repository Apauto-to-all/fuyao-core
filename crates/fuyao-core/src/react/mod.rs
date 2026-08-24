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
//! - AI 不调用工具（最终回复，一轮 ReAct 结束）：先 pending 全倒 guide，再 guide 全消费；
//!   消费到消息则回 ReAct 顶部再调一轮 LLM（下一轮 ReAct 循环），双队列都空才结束 turn
//!
//! 入队时机（三处，全走 `handle_inbound_user` 纯入队）：task idle 时主循环 select! 的
//! inbound 分支 + turn 内两段 select!（流式期间 / 工具执行期间）——turn 运行期间到达的
//! 消息即时入队，保证上面的两个消费时机在真实链路上能看到它们，而不是滞留通道
//! 推迟到 turn 结束后才各开独立 turn。
//!
//! 中断通道与队列分离：Interrupt 走独立 `rx_interrupt`（mpsc），
//! select! 中断点只监听它——不会误取 User。
//!
//! 工具结果不走队列：它是 ReAct 循环内部中间产物，产生即落 DB（事件级落库）。

mod builders;
mod compression;
pub(crate) mod queue;
pub(crate) mod retry;
#[cfg(test)]
mod tests;
mod title;
pub(crate) mod turn;

use crate::emit::Emitter;
use crate::engine::types::SharedQueue;
use crate::engine::types::TurnPhase;
use crate::engine::types::TurnPhaseGuard;
use crate::interrupt::notify_idle;
use crate::tool_registry::ToolRegistry;
use fuyao_api::UserMessageMode;
use fuyao_api::message::output::InterruptMessage as OutputInterruptMessage;
use fuyao_api::message::output::UserMessage as OutputUserMessage;
use fuyao_api::{AgentDefinition, CompressionConfig, ControlCommand, SessionParams};
use fuyao_hooks::SharedHooks;
use fuyao_provider::{ProviderRegistry, StreamUsage};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc::Receiver;
use tokio::sync::watch;
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
    /// turn 相位发送端（与 SessionHandle.turn_phase_rx 的接收端同源）
    ///
    /// 主循环进入 turn 区间（会写库：pre-turn 压缩 → 注入 → run_turn）时经
    /// [`TurnPhaseGuard`] 置 Running、区间结束回 Idle。Engine::stop_session 据
    /// 相位实现「等 turn 完全终止（含收尾落库）」的屏障语义。
    pub turn_phase: watch::Sender<TurnPhase>,
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
    /// 压缩的子 session 豁免判定、压缩重建 system_prompt 时按子/主推断 PromptUsage。
    ///
    /// 取代旧的"内存 `session.parent_session_id` 现读"——DB 唯一数据源后，
    /// session 不再常驻内存，此标记提升为 ctx 的不可变字段。
    pub is_child: bool,
    /// 标题生成判定门（每 session 至多开一次）
    ///
    /// 首批 user 消息注入时由 title 模块原子消耗（`swap` 换防），此后所有轮次
    /// 零成本跳过标题判定——含配置关闭 / 非首轮的情形
    /// （标题配置为进程级静态，首次判定即终局）。恒 false 起步，builder 不暴露 setter。
    pub title_gate: std::sync::atomic::AtomicBool,
}

/// SessionCtx 的可选字段覆盖链（由 [`SessionCtx::builder`] 进入，[`SessionCtxBuilder::build`] 收口）
///
/// 生产装配与测试共用：必填字段在 builder 入口以位置参数钉死（编译期强制，
/// 漏一个即编译失败），可选字段链式覆盖、`build` 兜底默认值。SessionCtx 增删
/// 字段时只需动 builder 与 build 两处，全部构造点自动跟随。
pub(crate) struct SessionCtxBuilder {
    // 必填字段（builder 入口已定，不可再改）
    store: Arc<fuyao_session::SessionStore>,
    providers: Arc<ProviderRegistry>,
    tools: Arc<ToolRegistry>,
    hooks: SharedHooks,
    agent_paths: fuyao_api::AgentPaths,
    definition: AgentDefinition,
    session_params: Arc<Mutex<SessionParams>>,
    emitter: Emitter,
    is_child: bool,
    // 可选字段（None = build 时取默认）
    guide: Option<SharedQueue>,
    pending: Option<SharedQueue>,
    compression_config: Option<CompressionConfig>,
    shutdown_token: Option<CancellationToken>,
    turn_phase: Option<watch::Sender<TurnPhase>>,
    subagent_ops: Option<Option<std::sync::Weak<dyn fuyao_api::SubagentOps>>>,
}

impl SessionCtxBuilder {
    /// 覆盖引导队列（默认空队列）
    pub(crate) fn guide(mut self, guide: SharedQueue) -> Self {
        self.guide = Some(guide);
        self
    }

    /// 覆盖排队队列（默认空队列）
    pub(crate) fn pending(mut self, pending: SharedQueue) -> Self {
        self.pending = Some(pending);
        self
    }

    /// 覆盖压缩配置（默认 `CompressionConfig::default()`；生产从全局配置显式传入）
    pub(crate) fn compression_config(mut self, config: CompressionConfig) -> Self {
        self.compression_config = Some(config);
        self
    }

    /// 覆盖关闭信号（默认新建 token；生产传引擎级 shutdown 的 child_token）
    pub(crate) fn shutdown_token(mut self, token: CancellationToken) -> Self {
        self.shutdown_token = Some(token);
        self
    }

    /// 覆盖 turn 相位发送端（默认新建 watch 通道，初值 Idle；生产传装配期创建的 sender）
    pub(crate) fn turn_phase(mut self, tx: watch::Sender<TurnPhase>) -> Self {
        self.turn_phase = Some(tx);
        self
    }

    /// 覆盖子 session 派生能力弱引用（默认 None；生产传引擎的 Weak）
    pub(crate) fn subagent_ops(
        mut self,
        ops: Option<std::sync::Weak<dyn fuyao_api::SubagentOps>>,
    ) -> Self {
        self.subagent_ops = Some(ops);
        self
    }

    /// 收口构造：可选字段取默认（空队列 / 默认压缩配置 / 新 token / None 弱引用），
    /// `last_usage` 恒 None 起步、`title_gate` 恒未消耗——两者无 setter
    pub(crate) fn build(self) -> SessionCtx {
        SessionCtx {
            store: self.store,
            providers: self.providers,
            tools: self.tools,
            hooks: self.hooks,
            agent_paths: self.agent_paths,
            definition: self.definition,
            session_params: self.session_params,
            emitter: self.emitter,
            guide: self.guide.unwrap_or_else(|| {
                Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()))
            }),
            pending: self.pending.unwrap_or_else(|| {
                Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()))
            }),
            last_usage: Arc::new(Mutex::new(None)),
            compression_config: self.compression_config.unwrap_or_default(),
            shutdown_token: self.shutdown_token.unwrap_or_default(),
            turn_phase: self
                .turn_phase
                .unwrap_or_else(|| watch::channel(TurnPhase::Idle).0),
            subagent_ops: self.subagent_ops.flatten(),
            is_child: self.is_child,
            title_gate: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl SessionCtx {
    /// SessionCtx 的唯一构造入口（生产 assemble_session 与测试共用）
    ///
    /// 九个必填字段作位置参数——编译期强制，漏一个即编译失败（fail-loud，无运行时
    /// 默认兜底）。可选字段（双队列 / 压缩配置 / 关闭信号 / 子代理弱引用）经
    /// [`SessionCtxBuilder`] 链式覆盖，见各 setter 的默认值说明。
    ///
    /// 参数超限是刻意选择：换成「必填字段聚合结构体」会退化成 SessionCtx 的字段
    /// 镜像（又一处需手动同步的字面量），位置参数才能让漏传直接编译失败。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn builder(
        store: Arc<fuyao_session::SessionStore>,
        providers: Arc<ProviderRegistry>,
        tools: Arc<ToolRegistry>,
        hooks: SharedHooks,
        agent_paths: fuyao_api::AgentPaths,
        definition: AgentDefinition,
        session_params: Arc<Mutex<SessionParams>>,
        emitter: Emitter,
        is_child: bool,
    ) -> SessionCtxBuilder {
        SessionCtxBuilder {
            store,
            providers,
            tools,
            hooks,
            agent_paths,
            definition,
            session_params,
            emitter,
            is_child,
            guide: None,
            pending: None,
            compression_config: None,
            shutdown_token: None,
            turn_phase: None,
            subagent_ops: None,
        }
    }
}

/// session 执行流的入站通道集合
///
/// 聚合喂给 session task 的三条接收端，由 Engine 的 assemble_session 一次性构造、
/// 整个 task 期间由 run_session 独占消费：inbound / interrupt / control 在主循环
/// select! 与 run_turn 间 `&mut` 借用。
///
/// 与 [`SessionCtx`]（共享依赖视图）正交：ctx 是所有 turn 复用的只读依赖，rx 是本 task
/// 独占消费的入站通道——两者一并构成 [`run_session`] 的全部入参，取代原先 18 个位置
/// 参数（调用方拆包、入口再打包的 ctx 的两份需手动同步的参数表）。
pub(crate) struct SessionRx {
    /// 入站用户消息（经管道分流入 guide / pending 队列）
    pub inbound: Receiver<OutputUserMessage>,
    /// 中断信号（idle 段与流式 / 工具执行段的中断点）
    pub interrupt: Receiver<OutputInterruptMessage>,
    /// 控制命令（手动压缩 / 回退等 B 类信号，turn 边界消费）
    pub control: Receiver<ControlCommand>,
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
pub(crate) async fn run_session(ctx: SessionCtx, rx: SessionRx) {
    tracing::info!(
        session_id = %ctx.emitter.session_id(),
        is_child = ctx.is_child,
        "session 执行流启动"
    );

    // 拆出入站三通道：inbound / interrupt / control 在主循环 select! 与 run_turn 间
    // &mut 借用。三个绑定均 mut——recv/try_recv 需 &mut self。
    let SessionRx {
        inbound: mut rx_inbound,
        interrupt: mut rx_interrupt,
        control: mut rx_control,
    } = rx;

    // 主循环：从 guide 全取消息 → 注入 → 跑 ReAct（自包含循环）。
    //
    // 消费许可状态机由 `TurnOutcome` 自带（may_consume / resume_on_new_intent，
    // 唯一权威定义见 turn.rs 的类型文档）；本循环只做无策略驱动：
    // 顶部按许可消费、idle 收到新用户消息时恢复许可。
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
        if outcome.may_consume() {
            // task 空闲时（无活跃 turn）= 无进行中的 ReAct 链，pending 的"等链结束"解禁条件已满足
            // → 此时 pending 与 guide 语义等价，立即解禁进 guide 触发新 turn
            // （否则只发 pending 时 pending 会死信，永远进不了 turn）
            let mut msgs = queue::consume_all_guide(&ctx.guide);
            if msgs.is_empty() {
                queue::drain_pending_to_guide(&ctx.guide, &ctx.pending);
                msgs = queue::consume_all_guide(&ctx.guide);
            }
            if !msgs.is_empty() {
                // === turn 相位守卫 ===
                // 本区块会写库（pre-turn 压缩 → 注入 → run_turn 全程，含中断收尾补发）：
                // 进入前置 Running、区块结束（含提前 return / panic）回 Idle。
                // Engine::stop_session 据相位等待——相位回 Idle 即本 session DB 已静默。
                let _turn_phase = TurnPhaseGuard::enter(&ctx.turn_phase);
                // === 上下文压缩检查（pre-turn）===
                // 同步执行：调一次 LLM(tools=[]) 拿摘要 → mark_compaction 落库
                // 失败 log warn 跳过本次压缩，主流程继续
                compression::run_pre_turn_compression(&ctx).await;

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
                // 首条内容先留一份（inject 调用会拿走 msgs 所有权），供标题旁路直取
                let title_seed = msgs.first().map(|m| m.payload.content.clone());
                // 一次性全部注入：每条经统一历史入口（拦截 → 投影落 DB → 发送 → 观察）
                crate::history::inject_user_messages(&ctx, msgs).await;
                // 首轮 user 消息落库后立即触发标题生成（fire-and-forget，不等 AI 回复）：
                // 在 run_turn 之前判定，解决旧逻辑「等 AI 整轮回复完成才生成」的延迟硬伤。
                // 判定门每 session 只开一次，内容取自本批首条（不回读 DB）
                title::maybe_spawn_title(&ctx, title_seed.as_deref()).await;
                // run_turn 自包含跑完整个队列直到空、或被控制命令/中断打断 → return TurnOutcome。
                // turn 运行期间到达的入站消息由 run_turn 内两段 select! 即时入队（见 turn.rs），
                // 不滞留通道；outcome 决定下一轮 loop 顶部的消费许可：非 Completed 则跳过 consume 等恢复。
                outcome = turn::run_turn(
                    &ctx,
                    &mut rx_inbound,
                    &mut rx_interrupt,
                    &mut rx_control,
                    model_config,
                )
                .await;
            }
        }
        // === 等待（无条件）===
        // 三类情况都进这里：
        // ① guide 空（Completed 且无消息）② run_turn 非 Completed return（队列剩余被保留）
        // ③ 消费被跳过（outcome 非 Completed）。
        // 停止消费：非 Completed 时 guide 剩余不跑，落这里等。
        // 恢复消费：inbound 收到新用户消息 → 恢复许可 → 回顶部 consume，
        // 旧剩余 + 新消息一起跑（忠实消费，不清队列）。
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
                // 恢复消费许可：新消息 = 新意图，回顶部 consume（旧剩余 + 新消息一起跑）
                outcome.resume_on_new_intent();
                handle_inbound_user(&ctx, inbound).await;
            }
            Some(interrupt_msg) = rx_interrupt.recv() => {
                // idle 中断：无活跃 turn，只发通知事件（收尾协议归 interrupt 模块）
                notify_idle(&ctx, &interrupt_msg.payload).await;
            }
            Some(cmd) = rx_control.recv() => {
                // idle 时控制命令到达：执行（同 task 串行消费，天然互斥）。
                // select! 结束后回顶部 drain 取其余
                handle_control(&ctx, cmd).await;
            }
        }
    }
}

/// 处理入站 User 消息：**纯入队**（无任何 side effect）
///
/// 入队只负责按 mode 分流到 guide / pending 队列，**不做**拦截、不发事件、不触发钩子。
/// 所有处理（拦截 / 落库 / 发送 / 观察）推迟到消费时刻统一过
/// `crate::history::inject_user_messages`——与 assistant / tool_result 走完全相同的路径。
///
/// 这样保证 user 消息的拦截/落库/发送三个时机**对齐**（都在消费时刻），
/// 修复"以输入消息为核心组织"导致的三时机错位（拦截提前、发送提前、落库延迟）。
///
/// 三处调用（同一 task 串行消费，天然互斥）：
/// - 主循环 idle select! 的 inbound 分支（task 空闲时入队，附带恢复消费许可）
/// - turn.rs 流式期间 / 工具执行期间两段 select! 的 inbound 分支（turn 运行期间即时入队，
///   不打断 turn，由 turn 内消费时机接管）
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
        ControlCommand::Compress => compression::run_manual_compression(ctx).await,
    }
}
