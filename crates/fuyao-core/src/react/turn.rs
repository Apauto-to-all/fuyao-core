//! 单轮 ReAct 循环
//!
//! 一个 turn = 处理一批已注入的 user messages，驱动"想 → 调工具 → 再想"循环，
//! 直到 AI 不再调工具（最终回复）且 guide/pending 都空才结束。
//!
//! 两个消费时机：
//! - **一批工具全部执行完成后、发回 AI 前**：只看 guide（还在调工具，pending 不动）。
//!   guide 全取注入 → continue；guide 空 → continue（只带工具结果）。
//! - **AI 不调用工具（最终回复，一轮 ReAct 结束）**：固定顺序
//!   ① pending 全部倒进 guide ② guide 全部消费注入 messages；
//!   消费到消息 → 回 ReAct 顶部再调一轮 LLM（下一轮 ReAct 循环），
//!   双队列都空才结束 turn。
//!
//! turn 运行期间的入站消息：两段 select! 均监听 `rx_inbound`（idle 段在 run_session
//! 外层），收到即经 `handle_inbound_user` 纯入队——不打断流式 / 工具执行，入队后
//! 由上述两个消费时机接管。入站通道若只在 idle 消费，turn 运行期间的消息到不了
//! 队列，两个消费时机在生产链路上永远空转（消费被推迟到 turn 结束后）。
//!
//! 中断：两段 select!——流式期间、工具执行期间。idle 段在 run_session 外层。
//! 中断分支用 `Some(...)` 模式：中断通道关闭（所有 tx drop）时分支禁用而非当作事件，
//! 调用方无需为保证通道打开而持有 tx。
//! 中断收尾协议（通知 + 部分结果 / 未完成 tool_call 的差集补发落库）归 interrupt
//! 模块，本文件的 select! 臂只负责判定命中哪种信号并触发对应入口，结束本轮。
//!
//! shutdown：两段 select! 各有 `biased` 优先的 shutdown 分支（优先于 interrupt），
//! 命中后与 interrupt 共用同一条收尾协议（`interrupt::shutdown_payload` 提供
//! source=Shutdown 的载荷），把已累积的部分结果落库后立即 return。retry.rs 的
//! 退避 sleep 也监听 shutdown_token，收到信号立即冒泡 Cancelled 让本层 shutdown
//! 分支接管。这样 shutdown 不再依赖 10s abort 兜底。

use super::SessionCtx;
use super::builders::{
    ResolvedModel, assistant_payload, build_chat_request, resolve_model, tool_call_data_to_event,
    tool_call_event_to_data,
};
use super::handle_control;
use super::handle_inbound_user;
use crate::interrupt::{self, SharedTurnState, TurnState};
use crate::react::queue;
use crate::stream::StreamResult;
use crate::tool_exec;
use fuyao_api::ModelConfig;
use fuyao_api::TurnDirective;
use fuyao_api::message::EventBase;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::output::{
    AssistantMessage, InterruptMessage as OutputInterruptMessage, UserMessage as OutputUserMessage,
};
use fuyao_provider::Provider;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc::Receiver;

/// run_turn 退出原因——自带主循环的消费许可状态机
///
/// 只传决策，不传消息：错误 / 中断的具体内容已通过 `OutputEvent`（Error / Interrupt）
/// 流给上层，本枚举只表达「主循环要不要继续消费 guide/pending」这一决策。
///
/// # 消费许可状态机（唯一权威定义，主循环是无策略驱动器）
///
/// - `Completed`（双队列跑空、AI 给最终回复）：[`may_consume`](Self::may_consume) 为真，
///   主循环继续 consume（本就空）或落 select! 等待
/// - 非 `Completed`（命令停 / 中断 / 失败）：`may_consume` 为假 → 主循环跳过 consume，
///   guide/pending 剩余**原样保留**（引擎不清队列），落 select! 等待
/// - **恢复迁移**：idle select! 的 inbound 分支收到新用户消息时调
///   [`resume_on_new_intent`](Self::resume_on_new_intent) 重置为 `Completed`——新消息 =
///   新意图，回顶部 consume 把「旧剩余 + 新消息」一起跑（忠实消费）
/// - **空闲解禁**：task 空闲（无活跃 turn）时 pending 的「等链结束」解禁条件已满足，
///   主循环顶部先倒 pending 再消费（否则只发 pending 会死信）——这是主循环侧的固定
///   动作，不属于本类型，但依赖 `may_consume` 为真才执行
///
/// 状态机的全部许可判定与迁移都经本类型的方法发生；修改消费语义只需动这里。
#[derive(Debug, Clone, Copy)]
pub(crate) enum TurnOutcome {
    /// 正常完成：双队列跑空，AI 给了最终回复
    Completed,
    /// 被 StopTurn 命令打断（回退 / 手动压缩）。DB 已被命令改写，turn 持有状态失效
    HaltedByCommand,
    /// 被用户中断打断
    Interrupted,
    /// 配置错误 / LLM 失败（不可恢复）
    Failed,
}

impl TurnOutcome {
    /// 消费许可：仅 `Completed` 允许主循环消费 guide/pending
    ///
    /// 非 `Completed` 的退出（命令停 / 中断 / 失败）都意味着「队列剩余不该继续跑」，
    /// 返回假让主循环跳过 consume、保留剩余等待恢复。
    pub(crate) fn may_consume(&self) -> bool {
        matches!(self, Self::Completed)
    }

    /// 新用户消息入队：恢复消费许可
    ///
    /// 新消息 = 新意图，重置为 `Completed`，主循环回顶部把「旧剩余 + 新消息」
    /// 一起消费（忠实消费，引擎不清队列）。由 idle select! 的 inbound 分支调用。
    pub(crate) fn resume_on_new_intent(&mut self) {
        *self = Self::Completed;
    }
}

/// 运行一轮 ReAct（user messages 已由 run_session 主循环经 inject_messages 注入 DB）
///
/// `model_config` 取自 session 的 SessionParams 快照（决定 model/options），turn 内多轮复用。
/// `rx_inbound` 为入站通道接收端：流式与工具执行两段 select! 监听它，收到 User 消息
/// 即经 `handle_inbound_user` 纯入队（不打断 turn），由消费时机接管——保证 turn 运行
/// 期间到达的消息能赶上前面的消费点，而不是滞留通道等到 turn 结束。
/// `rx_interrupt` 为中断通道接收端，两段 select! 监听它。
/// `rx_control` 为控制通道接收端，ReAct loop 顶部间隙检查点消费它——取到任意 StopTurn
/// 命令（手动压缩 / 回退）则立即 return，打断 ReAct 链让命令快速生效（命令自身的 DB
/// 写已在 handle_control 内完成，无需额外落库）。
///
/// 返回 [`TurnOutcome`]：主循环据此决定是否继续消费队列。非 `Completed` 的退出都意味着
/// 「队列剩余不该继续跑」，主循环应跳过 consume 落 select! 等用户新消息恢复。
pub(crate) async fn run_turn(
    ctx: &SessionCtx,
    rx_inbound: &mut Receiver<OutputUserMessage>,
    rx_interrupt: &mut Receiver<OutputInterruptMessage>,
    rx_control: &mut Receiver<fuyao_api::ControlCommand>,
    model_config: ModelConfig,
) -> TurnOutcome {
    // 解析本轮 model_id + 从 registry 查 Provider 实例
    // 任一失败：发 Error 事件 + 结束本轮（配置错误，永久不可恢复）
    //
    // is_child 取自 ctx（创建时由 session 行的 parent_session_id 定死）：子 session 的
    // 工具列表过滤掉 child_invisible 的工具（递归防护——子 session 看不到派生类工具）
    let resolved: ResolvedModel = match resolve_model(
        &model_config,
        &ctx.tools,
        ctx.is_child,
        &ctx.definition.tools,
        &ctx.agent_paths,
    ) {
        Ok(r) => r,
        Err(msg) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                cause = %msg,
                "模型解析失败（model_id 无效或为空）"
            );
            emit_unrecoverable_error(ctx, &msg).await;
            return TurnOutcome::Failed;
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
            emit_unrecoverable_error(ctx, &msg).await;
            return TurnOutcome::Failed;
        }
    };
    let model = resolved.model.clone();
    let options = resolved.options.clone();

    // 可见窗口的 keep_recent token 预算：按当前模型上下文比例算（与压缩侧同口径）
    // context_length 已由 resolve_model 一并解析（resolved.context_length）；模型未注册
    // 时为 None——保留预算按 0（仅影响已压缩会话的近期消息附带量），该模型的 LLM
    // 调用自会在 provider 处失败，无需此处兜底
    let keep_tokens = ctx
        .compression_config
        .effective_keep_tokens(resolved.context_length.unwrap_or(0));

    loop {
        // === 控制通道间隙检查点 ===
        // 每轮 ReAct 开始前非阻塞排空控制通道。时机安全：上一轮工具结果已落库、
        // 下一轮 LLM 还没调用——DB 稳定态。命令改 DB 后，紧接着的 build_chat_request
        // 读到最新状态（回退重算的 count / 压缩新边界）。
        //
        // 忠实执行：while try_recv 逐条 FIFO 排空，不做同批去重 / 合并（引擎是忠实执行器）。
        // 取到命令先问它的 turn_directive（命令自带，固有属性），任一条 StopTurn 即标记停止。
        // 全排空后若有任意 StopTurn → 立即 return（命令的 DB 写已在 handle_control 内完成），
        // 打断 ReAct 链交还控制权；通道空 / 全是 Continue → 继续 ReAct。
        //
        // stop_turn 标志的必要性：while try_recv 可能一次取到多条命令，必须遍历完才能判断
        // 「有没有出现过 StopTurn」，不用标志循环结束后无法回溯。
        let mut stop_turn = false;
        while let Ok(cmd) = rx_control.try_recv() {
            if matches!(cmd.turn_directive(), TurnDirective::StopTurn) {
                stop_turn = true;
            }
            handle_control(ctx, cmd).await;
        }
        if stop_turn {
            return TurnOutcome::HaltedByCommand;
        }

        // 本轮 LLM 调用的共享状态（中断分支读部分结果用）。
        // 每次 loop 顶部新建；retry.rs 在重试时清空复用，保证不携带上一次的部分结果。
        let state: SharedTurnState = Arc::new(std::sync::Mutex::new(TurnState::new()));
        // 本轮的 ChatRequest（重试间复用同一份——一次 LLM 调用内 DB 历史不变）
        // 消息已不在内存，每次构造时从 DB 查可见窗口（动态拼接，按 keep_tokens 截近期）
        let request =
            build_chat_request(ctx.store.as_ref(), ctx.emitter.session_id(), keep_tokens).await;

        // 中断点①：流式期间（含重试 sleep 期间——select! drop future 即取消 sleep）
        // run_stream_with_retry 内部按错误类型自动重试，发 OutputEvent::Retry 给 UI。
        // 中断：外层 select! drop retry future → 退避 sleep 取消 → 中断分支胜出。
        // shutdown：retry.rs 退避 sleep 期间收到 shutdown 信号会冒泡 Cancelled，
        //          本 select! 的 shutdown 分支也并发监听 token，谁先到谁接管。
        //
        // 入站分支：流式期间到达的 User 消息即时入队（纯入队，不打断流式）。
        // biased 顺序放在 retry_fut 之前——流已就绪时先入队再取流结果，保证
        // 「流式期间到达的消息」能被本轮 turn 后续的消费点（①/②）看到，
        // 而不是滞留通道变成 turn 结束后的独立新 turn。入队后 continue 重新
        // select! 继续等流式（retry_fut 已 pin，跨 select! 轮次复用）。
        let stream_result = {
            let retry_fut = super::retry::run_stream_with_retry(
                ctx, request, &model, &options, &provider, &state,
            );
            tokio::pin!(retry_fut);
            loop {
                tokio::select! {
                    biased;
                    // shutdown 优先（高于 interrupt）：收尾（通知 + 部分结果落库）后立即退出
                    _ = ctx.shutdown_token.cancelled() => {
                        interrupt::finish_streaming(ctx, &state, &interrupt::shutdown_payload()).await;
                        return TurnOutcome::Interrupted;
                    }
                    // 入站消息：纯入队后继续等流式。Some 模式：通道关闭（所有 tx drop）
                    // 时分支禁用，关闭不是事件、不参与调度
                    Some(inbound) = rx_inbound.recv() => {
                        handle_inbound_user(ctx, inbound).await;
                    }
                    result = &mut retry_fut => break result,
                    // 中断通道独立：此处只会收到 Interrupt。
                    // Some 模式：通道关闭（所有 tx drop）时本分支禁用，select 继续等流式 / shutdown
                    // ——关闭不是事件、不参与调度，也消除「关闭后 recv 立即就绪 + continue」的忙循环
                    Some(interrupt_msg) = rx_interrupt.recv() => {
                        interrupt::finish_streaming(ctx, &state, &interrupt_msg.payload).await;
                        return TurnOutcome::Interrupted;
                    }
                }
            }
        };

        match stream_result {
            Ok(result) => {
                if result.tool_calls.is_empty() {
                    // 无工具调用：最终回复。消费时机②：pending 倒 guide 后全取注入，
                    // 消费到消息 → 回 ReAct 顶部再调一轮 LLM（下一轮 ReAct 循环）；
                    // 双队列都空 → turn 正常结束
                    if !handle_final_reply(ctx, &result, &model_config).await {
                        return TurnOutcome::Completed;
                    }
                } else {
                    // 有工具调用：发 AssistantMessage → 执行整批工具 → 消费时机①
                    // 返回 true 表示执行期间被 shutdown / interrupt 打断（已落库），需退出 turn
                    let halted =
                        handle_tool_calls(ctx, rx_inbound, rx_interrupt, &result, &model_config)
                            .await;
                    if halted {
                        return TurnOutcome::Interrupted;
                    }
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
                emit_unrecoverable_error(ctx, &format!("LLM 调用失败: {e}")).await;
                return TurnOutcome::Failed;
            }
        }
    }
}

/// 发「不可恢复错误」事件（`recoverable: false`）
///
/// 统一构造本文件三类失败的通知事件：model_id 解析失败 / Provider 实例未注册（配置错误）
/// 与 LLM 调用失败（retry 已判定不可重试或耗尽）——三者共用 Error 通道，
/// `message` 由各调用点精准指向错误源；`tracing::warn` 留在调用点（各处语义不同）。
async fn emit_unrecoverable_error(ctx: &SessionCtx, message: &str) {
    let error_event = OutputEvent::Error(fuyao_api::message::output::ErrorMessage {
        base: EventBase::default(),
        payload: fuyao_api::message::output::ErrorPayload {
            message: message.to_string(),
            recoverable: false,
        },
    });
    crate::dispatch::dispatch(&ctx.emitter, &ctx.hooks, error_event).await;
}

/// 处理最终回复（AI 不调用工具，一轮 ReAct 结束）
///
/// 固定顺序：① pending 全部倒进 guide（追加在 guide 现有内容之后）② guide 全部
/// 消费注入 messages。
///
/// 返回值表达 turn 是否还有后续：
/// - `false`：guide 和 pending 都空，turn 正常结束（消息已在统一历史入口事务内即时落库）
/// - `true`：消费到消息并已注入——调用方应回 ReAct 顶部再调一轮 LLM，
///   让 AI 真正回应这批消息（触发下一轮 ReAct 循环），而不是注入后无人应答
async fn handle_final_reply(
    ctx: &SessionCtx,
    result: &StreamResult,
    model_config: &ModelConfig,
) -> bool {
    // 回传本轮真实 usage 给主循环（pre-turn 压缩触发判定用）
    *ctx.last_usage.lock().await = Some(result.usage.clone());

    // 经计费入口：拦截 → 事件投影 Message（token/cost/模型归属内化）→ 落 DB → 发送事件
    let event = OutputEvent::Assistant(AssistantMessage {
        base: EventBase::default(),
        payload: assistant_payload(result),
    });
    crate::history::emit_billed_to_history(ctx, event, model_config.model_id.as_str()).await;
    // 拦截 Block：消息不进历史、不计费——插件的责任，引擎不替它兜底

    // 消费时机②：① pending 全倒 guide ② guide 全取注入
    queue::drain_pending_to_guide(&ctx.guide, &ctx.pending);
    let msgs = queue::consume_all_guide(&ctx.guide);
    if msgs.is_empty() {
        false
    } else {
        // 有消息：全部注入（每条经统一历史入口拦截→落 DB→发送→观察）
        crate::history::inject_user_messages(ctx, msgs).await;
        true
    }
}

/// 处理工具调用：逐个拦截工具调用 → 计费入口同步 AssistantMessage → 执行整批工具 → 消费时机①
///
/// 返回 `true` 表示执行期间被 shutdown / interrupt 打断（已 emit 事件 + 落库），调用方应据此
/// 退出 turn；返回 `false` 表示整批工具正常完成，调用方可继续 ReAct 下一轮。
///
/// 工具调用两层拦截模型（清晰边界）：
/// - **第一层：ToolCall 事件逐个拦截**（粒度细）：插件可独立 Block 单个 tool_call 或改其 args。
///   拦截后的 effective_tool_calls 作为「执行输入」+「存储字段」的权威数据源。
/// - **第二层：AssistantMessage 事件整体拦截**（粒度粗）：插件可改 content / reasoning
///   等内容字段。但 tool_calls 字段**以 effective_tool_calls 为准**——避免「存储用的 tool_calls」
///   与「执行的 tool_calls」分裂。如需改 tool_calls，请在第一层 ToolCall 拦截时改。
///
/// 工具执行通过 channel 通知完成，turn.rs 边收边走统一历史入口（拦截 → 落 DB → 发事件）。
/// 中断时 channel 里剩余结果也清空落库，保证不丢。
///
/// `rx_inbound`：工具执行段 select! 监听入站通道，收到 User 消息即时纯入队（见循环内注释）。
async fn handle_tool_calls(
    ctx: &SessionCtx,
    rx_inbound: &mut Receiver<OutputUserMessage>,
    rx_interrupt: &mut Receiver<OutputInterruptMessage>,
    result: &StreamResult,
    model_config: &ModelConfig,
) -> bool {
    // 步骤1：逐个拦截 ToolCall 事件，构造 effective_tool_calls
    // 整批 tool_calls 拆成单个 ToolCall 事件各自拦截；Block 的跳过。
    let mut effective_tool_calls: Vec<fuyao_api::ToolCallData> =
        Vec::with_capacity(result.tool_calls.len());
    for tc in &result.tool_calls {
        let event = tool_call_data_to_event(tc);
        if let Some(intercepted) = crate::dispatch::intercept(&ctx.hooks, event) {
            // 拦截通过：先从拦截后的 payload 提取工具调用数据回灌，再发送（含观察）——
            // deliver 按值消费事件，提取必须在发送前完成
            if let Some(data) = tool_call_event_to_data(&intercepted) {
                effective_tool_calls.push(data);
            }
            crate::dispatch::deliver(&ctx.emitter, &ctx.hooks, intercepted).await;
        }
        // Block：跳过该工具（不发送、不执行、不存储）
    }

    // 步骤2：用 effective_tool_calls 构造 effective_result → AssistantMessage 事件
    // 经计费入口：拦截整个 AssistantMessage（同步 content/reasoning）→ 投影落 DB → 发送
    // （tool_calls 落库取自拦截后事件 payload，与第一层 ToolCall 拦截结果同源）
    let effective_result = StreamResult {
        text: result.text.clone(),
        reasoning: result.reasoning.clone(),
        tool_calls: effective_tool_calls,
        usage: result.usage.clone(),
    };
    let event = OutputEvent::Assistant(AssistantMessage {
        base: EventBase::default(),
        payload: assistant_payload(&effective_result),
    });
    crate::history::emit_billed_to_history(ctx, event, model_config.model_id.as_str()).await;
    // 拦截 Block：消息不进历史、不计费——插件的责任

    // 若全部工具调用被拦截（effective 为空）或 AssistantMessage 被 Block，无需执行
    if effective_result.tool_calls.is_empty() {
        let msgs = queue::consume_all_guide(&ctx.guide);
        if !msgs.is_empty() {
            crate::history::inject_user_messages(ctx, msgs).await;
        }
        return false;
    }

    // 步骤3：中断点②——工具执行期间（含 shutdown）
    // execute_tools 通过 result_tx 通知完成（一个一个通知）；本循环边收边走统一历史入口
    // （拦截 → 落 DB → 发送事件）并记入已答集。中断/shutdown 时通道里已缓冲的已完成
    // 结果也落库（不丢），未完成的经 interrupt 模块按差集补发中断式 ToolResult——
    // 通知 + 补发的收尾协议归 interrupt 模块，本循环只负责触发。
    //
    // 已答集（answered）是未完成判定的内存真相源：逐条观测结果通道即精确，不查 DB、
    // 不依赖可见窗口（窗口截断会把已完成误判成未完成而重复补发，同一 tool_call_id
    // 出现两条结果会让下轮 LLM 调用协议报错）。已答 = 已经过历史入口处理——含插件
    // Block（插件的责任，引擎不补偿）与落库失败（沿用「落库失败已丢弃」降级）。
    let tool_calls_for_exec = effective_result.tool_calls.clone();
    let (result_tx, mut result_rx) =
        tokio::sync::mpsc::channel::<tool_exec::ToolExecResult>(tool_calls_for_exec.len());
    // 派生 child_token：shutdown 时 parent→child 自动传播；interrupt 分支显式 cancel。
    // handler 据此优雅收尾长任务（杀子进程等），不监听的靠 abort 兜底（双保险）
    let cancel = ctx.shutdown_token.child_token();
    // 任务列表存储能力：会话存储实现了 TodoStoreOps，coerce 成 trait object 注入工具 ctx。
    // todo 工具据此读写当前 session 的任务列表，不再自建连接池。
    let todo_store: std::sync::Arc<dyn fuyao_api::TodoStoreOps> = ctx.store.clone();
    // 聚合工具执行级注入句柄：tools / agent_paths / emitter / cancel + 能力聚合
    // （subagent_ops / event_forwarder（emitter 派生）/ todo_store）。tool_calls 与
    // result_tx 随调用变化，仍作 execute_tools 的独立参数
    let exec_ctx = tool_exec::ToolExecCtx {
        tools: ctx.tools.clone(),
        agent_paths: ctx.agent_paths.clone(),
        emitter: ctx.emitter.clone(),
        cancel: cancel.clone(),
        capabilities: fuyao_api::ToolCapabilities {
            subagent_ops: ctx.subagent_ops.clone(),
            event_forwarder: Some(ctx.emitter.tx_clone()),
            todo_store: Some(todo_store),
        },
    };
    let exec_fut = tool_exec::execute_tools(&tool_calls_for_exec, &result_tx, &exec_ctx);
    tokio::pin!(exec_fut);

    // 本批已答集：已观测到结果并经历史入口处理的 tool_call_id
    let mut answered: HashSet<String> = HashSet::new();

    loop {
        tokio::select! {
            biased; // shutdown / 中断优先，保证及时响应
            // shutdown 优先（高于 interrupt）：先清空已完成结果（不丢），再走收尾协议退出
            // 工具执行 fut 被 drop → JoinSet drop → tokio 自动 abort 所有未完成工具 task
            _ = ctx.shutdown_token.cancelled() => {
                drain_finished_results(ctx, &mut result_rx, &mut answered).await;
                interrupt::finish_tool_batch(
                    ctx,
                    &effective_result.tool_calls,
                    &answered,
                    &interrupt::shutdown_payload(),
                )
                .await;
                return true;
            }
            Some(interrupt_msg) = rx_interrupt.recv() => {
                // interrupt 命中：显式 cancel 工具批 child_token（shutdown 靠 parent 传播，无需此处 cancel）
                // 让监听 token 的长任务 handler 后台优雅收尾；无宽限期，立即清空 + 收尾。
                // Some 模式：通道关闭时本分支禁用，工具批正常跑完——关闭不模拟用户中断
                cancel.cancel();
                drain_finished_results(ctx, &mut result_rx, &mut answered).await;
                interrupt::finish_tool_batch(
                    ctx,
                    &effective_result.tool_calls,
                    &answered,
                    &interrupt_msg.payload,
                )
                .await;
                return true;
            }
            Some(r) = result_rx.recv() => {
                // 完成一个：立即走统一历史入口（拦截 → 落 DB → 发送事件）并记入已答集
                record_tool_result(ctx, &mut answered, r).await;
            }
            // 入站消息：纯入队，不打断工具批。biased 放在 exec_fut 之前——整批工具
            // 完成与入站消息同时就绪时先入队再收批，保证紧随工具完成的消费时机①
            // 能看到这条消息（同 turn 消费，而非推迟到 turn 结束后的独立新 turn）。
            // Some 模式：通道关闭（所有 tx drop）时分支禁用
            Some(inbound) = rx_inbound.recv() => {
                handle_inbound_user(ctx, inbound).await;
            }
            _ = &mut exec_fut => {
                // execute_tools 完成：清空 channel 里剩余的（防丢，理论已空）
                drain_finished_results(ctx, &mut result_rx, &mut answered).await;
                break;
            }
        }
    }

    // 步骤4：消费时机①——一批工具全部完成后、发回 AI 前，只看 guide（pending 不动）
    let msgs = queue::consume_all_guide(&ctx.guide);
    if !msgs.is_empty() {
        crate::history::inject_user_messages(ctx, msgs).await;
    }
    // 回 run_turn 顶部：带 guide 消息（若有）+ 工具结果再调 LLM
    false
}

/// 记录一条已完成的工具结果：经统一入口落 DB 并记入已答集
///
/// 工具完成时立即调用：拦截 → 投影 Message（含 tool_name）→ 落 DB → 发送事件 → 观察，
/// 保证「拦截→存储→发送」三者一致；id 同步记入已答集，供中断收尾做未完成判定。
/// id 先于落库 await 记入（中断信号不会打断本函数——select! 臂的处理函数跑完才重新调度），
/// 且 Block / 落库失败同样记入：插件决策不补偿，落库失败沿用「已丢弃」降级。
async fn record_tool_result(
    ctx: &SessionCtx,
    answered: &mut HashSet<String>,
    result: tool_exec::ToolExecResult,
) {
    answered.insert(result.tool_call_id.clone());
    let event = OutputEvent::ToolResult(fuyao_api::message::output::ToolResultMessage {
        base: EventBase::default(),
        payload: fuyao_api::message::output::ToolResultPayload {
            tool_call_id: result.tool_call_id,
            tool_name: result.tool_name,
            content: result.content,
        },
    });
    crate::history::emit_to_history(ctx, event).await;
}

/// 非阻塞清空结果通道：已完成结果逐条落库并记入已答集
///
/// 中断 / shutdown / 正常完成三路共用——结果通道里已缓冲的结果都不丢。
/// 用 try_recv 非阻塞清空（exec_fut 可能还在跑，recv 会阻塞）。
async fn drain_finished_results(
    ctx: &SessionCtx,
    result_rx: &mut Receiver<tool_exec::ToolExecResult>,
    answered: &mut HashSet<String>,
) {
    while let Ok(r) = result_rx.try_recv() {
        record_tool_result(ctx, answered, r).await;
    }
}
