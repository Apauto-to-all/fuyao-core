//! 单轮 ReAct 循环
//!
//! 一个 turn = 处理一批已注入的 user messages，驱动"想 → 调工具 → 再想"循环，
//! 直到 AI 不再调工具（最终回复）且 guide/pending 都空才结束。
//!
//! turn 内两个消费时机（工具批完成后 / 最终回复后）的取队规则与消费许可由
//! [`crate::react::queue::ConsumeGate`] 权威定义，本文件只在对应场合向它取件；
//! 取出的批次统一走 `consume_batch`（连续 User 段注入、Control 就地执行）——
//! 注入过 User 消息则回 ReAct 顶部再调一轮 LLM（下一轮 ReAct 循环），
//! 双队列都空才结束 turn。
//!
//! turn 运行期间的入站条目：两段 select! 监听 `rx_inbound`（外部 User / Control 条目
//! 与插件注入的 User 条目，经 `handle_inbound_item` 纯入队）——不打断流式 / 工具执行，
//! 入队后由上述两个消费时机接管。入站通道若只在 idle 消费，turn 运行期间的消息
//! 到不了队列，两个消费时机在生产链路上永远空转（消费被推迟到 turn 结束后）。
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
use super::consume_batch;
use super::handle_inbound_item;
use crate::interrupt::{self, SharedTurnState, TurnState};
use crate::react::queue::{ConsumeGate, ConsumeTiming};
use crate::stream::StreamResult;
use crate::tool_exec;
use fuyao_api::ModelConfig;
use fuyao_api::message::EventBase;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::QueueEntry;
use fuyao_api::message::output::{AssistantMessage, InterruptMessage as OutputInterruptMessage};
use fuyao_provider::Provider;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc::Receiver;

/// run_turn 退出原因
///
/// 只传决策，不传消息：错误 / 中断的具体内容已通过 `OutputEvent`（Error / Interrupt）
/// 流给上层，本枚举只表达「turn 为何结束」。主循环把退出原因喂给
/// [`ConsumeGate::on_turn_end`](crate::react::queue::ConsumeGate::on_turn_end)
/// 迁移消费许可——`Completed` 开放许可继续消费，中断 / 失败暂停消费并保留
/// 队列剩余；消费时机的权威定义在 [`crate::react::queue`] 模块。
#[derive(Debug, Clone, Copy)]
pub(crate) enum TurnOutcome {
    /// 正常完成：双队列跑空，AI 给了最终回复
    Completed,
    /// 被用户中断打断（含 shutdown 共用的收尾路径）
    Interrupted,
    /// 配置错误 / LLM 失败（不可恢复）
    Failed,
}

/// 运行一轮 ReAct（user messages 已由 run_session 主循环经批次处理注入 DB）
///
/// `model_config` 取自 session 的 SessionParams 快照（决定 model/options），turn 内多轮复用。
/// `rx_inbound` 为入站通道接收端（外部 User / Control 条目与插件注入的 User 条目）：
/// 流式与工具执行两段 select! 监听它，收到条目即经 `handle_inbound_item` 纯入队
/// （不打断 turn），由消费时机接管——保证 turn 运行期间到达的条目能赶上前面的
/// 消费点，而不是滞留通道等到 turn 结束。
/// `rx_interrupt` 为中断通道接收端，两段 select! 监听它。
/// `gate` 为双队列消费门：turn 内两个消费时机（工具批完成后 / 最终回复后）
/// 经它取件，取队规则的权威定义见 [`crate::react::queue`] 模块。
///
/// 返回 [`TurnOutcome`]：主循环经
/// [`ConsumeGate::on_turn_end`](crate::react::queue::ConsumeGate::on_turn_end)
/// 迁移消费许可——非 `Completed` 的退出意味着「队列剩余不该继续跑」。
///
/// turn 无论以哪种原因退出（正常完成 / 中断 / shutdown / 失败），收尾统一在本函数
/// 末尾做快照补拍（[`track_at_turn_end`]）——本 turn 有过工具批落账时再采一行，
/// 承载最后一批工具的变更窗口。
pub(crate) async fn run_turn(
    ctx: &SessionCtx,
    rx_inbound: &mut Receiver<QueueEntry>,
    rx_interrupt: &mut Receiver<OutputInterruptMessage>,
    model_config: ModelConfig,
    gate: &ConsumeGate,
) -> TurnOutcome {
    // 快照账跟踪：记录本 turn 的锚点线索，收尾补拍的输入
    let mut ledger = TurnLedger::default();
    let outcome = run_turn_loop(
        ctx,
        rx_inbound,
        rx_interrupt,
        model_config,
        gate,
        &mut ledger,
    )
    .await;
    track_at_turn_end(ctx, &ledger).await;
    outcome
}

/// turn 内快照账跟踪（收尾补拍的输入）
///
/// 快照行的 `files` 记「自上一行以来的差异」，由下一次采集落账——turn 最后一批
/// 工具的效果若无收尾行承载，回退会漏掉它。本结构在 turn 运行期间累积锚点线索：
/// - [`Self::last_assistant_seq`]：收尾行的锚点（turn 最后一条落库的 assistant 消息 seq）
/// - [`Self::tool_batch_tracked`]：本 turn 是否有过带锚点的工具批采集——纯对话 turn
///   不补拍（无批次即无变更窗口要承载）
#[derive(Default)]
struct TurnLedger {
    /// 本 turn 最后一条成功落库的 assistant 消息 seq
    /// （拦截 Block / 落库失败返回 None 时不覆盖已有值——锚点必须指向真实落库的行）
    last_assistant_seq: Option<i64>,
    /// 本 turn 是否有过带锚点的工具批采集（assistant 消息已落库且本批将执行工具）
    tool_batch_tracked: bool,
}

/// ReAct 循环主体（[`run_turn`] 的内层执行体）
///
/// 参数与返回值同 [`run_turn`]，多一个 `ledger`：turn 运行期间由
/// [`handle_final_reply`] / [`handle_tool_calls`] 累积快照账锚点线索，
/// 供外层收尾补拍消费。
async fn run_turn_loop(
    ctx: &SessionCtx,
    rx_inbound: &mut Receiver<QueueEntry>,
    rx_interrupt: &mut Receiver<OutputInterruptMessage>,
    model_config: ModelConfig,
    gate: &ConsumeGate,
    ledger: &mut TurnLedger,
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
            // 含完整实体标识（provider_id + model_id）与可用列表：供应商删除 /
            // 反注册后的下一轮在此报错，用户与上层（选择器回退兜底）据此定位
            let msg = format!(
                "Provider '{}' 未注册（model_id: '{}'，可用: {:?}）——供应商可能已被删除或未运行时注册，\
                 相关会话需切换到可用模型",
                resolved.provider_id,
                resolved.model_id,
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

    loop {
        // 本轮 LLM 调用的共享状态（中断分支读部分结果用）。
        // 每次 loop 顶部新建；retry.rs 在重试时清空复用，保证不携带上一次的部分结果。
        let state: SharedTurnState = Arc::new(std::sync::Mutex::new(TurnState::new()));
        // 本轮的 ChatRequest（重试间复用同一份——一次 LLM 调用内 DB 历史不变）
        // 消息已不在内存，每次构造时从 DB 查可见窗口（动态拼接）
        let request = build_chat_request(ctx.store.as_ref(), ctx.emitter.session_id()).await;

        // 中断点①：流式期间（含重试 sleep 期间——select! drop future 即取消 sleep）
        // run_stream_with_retry 内部按错误类型自动重试，发 OutputEvent::Retry 给 UI。
        // 中断：外层 select! drop retry future → 退避 sleep 取消 → 中断分支胜出。
        // shutdown：retry.rs 退避 sleep 期间收到 shutdown 信号会冒泡 Cancelled，
        //          本 select! 的 shutdown 分支也并发监听 token，谁先到谁接管。
        //
        // 入站 / 插件分支：流式期间到达的条目即时入队（纯入队，不打断流式）。
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
                    // 入站条目（外部 User / Control 或插件注入的 User）：纯入队后继续
                    // 等流式。Some 模式：通道关闭（所有 tx drop）时分支禁用，关闭不是事件、不参与调度
                    Some(entry) = rx_inbound.recv() => {
                        handle_inbound_item(ctx, entry).await;
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
                    // 无工具调用：最终回复。消费时机②取件（规则见 queue 模块），
                    // 注入过消息 → 回 ReAct 顶部再调一轮 LLM（下一轮 ReAct 循环）；
                    // 双队列都空 → turn 正常结束
                    if !handle_final_reply(ctx, gate, &result, &model_config, ledger).await {
                        return TurnOutcome::Completed;
                    }
                } else {
                    // 有工具调用：发 AssistantMessage → 执行整批工具 → 消费时机①
                    // 返回 true 表示执行期间被 shutdown / interrupt 打断（已落库），需退出 turn
                    let halted = handle_tool_calls(
                        ctx,
                        gate,
                        rx_inbound,
                        rx_interrupt,
                        &result,
                        &model_config,
                        ledger,
                    )
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
/// 消费时机②：经消费门取件（pending 倒灌 guide 后全取，取队规则由
/// [`ConsumeTiming::FinalReply`] 内定），取出条目经 [`consume_batch`] 处理
/// （连续 User 段注入历史、Control 就地执行）。
///
/// 返回值表达 turn 是否还有后续：
/// - `false`：guide 和 pending 都空（未注入任何 User），turn 正常结束
///   （消息已在统一历史入口事务内即时落库）
/// - `true`：注入过消息——调用方应回 ReAct 顶部再调一轮 LLM，
///   让 AI 真正回应这批消息（触发下一轮 ReAct 循环），而不是注入后无人应答
async fn handle_final_reply(
    ctx: &SessionCtx,
    gate: &ConsumeGate,
    result: &StreamResult,
    model_config: &ModelConfig,
    ledger: &mut TurnLedger,
) -> bool {
    // 回传本轮真实 usage 给主循环（pre-turn 压缩触发判定用）
    *ctx.last_usage.lock().await = Some(result.usage.clone());

    // 经计费入口：拦截 → 事件投影 Message（token/cost/模型归属内化）→ 落 DB → 发送事件
    let event = OutputEvent::Assistant(AssistantMessage {
        base: EventBase::default(),
        payload: assistant_payload(result),
    });
    let seq =
        crate::history::emit_billed_to_history(ctx, event, model_config.model_id.as_str()).await;
    // 记锚点线索：最终回复是 turn 的末条 assistant 消息，收尾行锚定它
    // （None = Block / 落库失败，保留更早的锚点值）
    if seq.is_some() {
        ledger.last_assistant_seq = seq;
    }
    // 拦截 Block：消息不进历史、不计费——插件的责任，引擎不替它兜底

    // 消费时机②：经消费门取件（pending 倒灌 guide 后全取）
    let entries = gate.take(ConsumeTiming::FinalReply, &ctx.guide, &ctx.pending);
    consume_batch(ctx, entries).await
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
/// `rx_inbound`：工具执行段 select! 监听入站通道，收到条目（外部 User / Control
/// 或插件注入的 User）即时纯入队（见循环内注释）。
async fn handle_tool_calls(
    ctx: &SessionCtx,
    gate: &ConsumeGate,
    rx_inbound: &mut Receiver<QueueEntry>,
    rx_interrupt: &mut Receiver<OutputInterruptMessage>,
    result: &StreamResult,
    model_config: &ModelConfig,
    ledger: &mut TurnLedger,
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
    // 返回落库 seq：工具批的文件快照行以 assistant 消息 seq 为锚点，
    // Block / 落库失败返回 None（无锚点即跳过采集）
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
    let assistant_seq =
        crate::history::emit_billed_to_history(ctx, event, model_config.model_id.as_str()).await;
    // 记锚点线索：本批 assistant 消息（None = Block / 落库失败，保留更早的锚点值）
    if assistant_seq.is_some() {
        ledger.last_assistant_seq = assistant_seq;
    }
    // 拦截 Block：消息不进历史、不计费——插件的责任，引擎不替它兜底

    // 若全部工具调用被拦截（effective 为空）或 AssistantMessage 被 Block，无需执行
    // 工具批——直接走消费时机①（经消费门取件）后回 ReAct 顶部
    if effective_result.tool_calls.is_empty() {
        let entries = gate.take(ConsumeTiming::AfterToolBatch, &ctx.guide, &ctx.pending);
        consume_batch(ctx, entries).await;
        return false;
    }

    // 步骤2.5：文件快照采集——此刻本批 assistant 消息已落库（seq 已知）、工具尚未
    // 执行，工作区状态即「本批工具执行前」的基线。纯对话轮不进本函数，零快照成本；
    // 采集失败 WARN 不中断（fail-open：该批无快照行，工具照常执行）
    record_snapshot_row(ctx, assistant_seq).await;
    // 带锚点的采集已尝试：收尾补拍的前置条件成立（无锚点批不计数——无行承载也无收尾义务）
    ledger.tool_batch_tracked = ledger.tool_batch_tracked || assistant_seq.is_some();

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
            // 入站条目：纯入队，不打断工具批。biased 放在 exec_fut 之前——整批工具
            // 完成与入站条目同时就绪时先入队再收批，保证紧随工具完成的消费时机①
            // 能看到这条消息（同 turn 消费，而非推迟到 turn 结束后的独立新 turn）。
            // Some 模式：通道关闭（所有 tx drop）时分支禁用
            Some(entry) = rx_inbound.recv() => {
                handle_inbound_item(ctx, entry).await;
            }
            _ = &mut exec_fut => {
                // execute_tools 完成：清空 channel 里剩余的（防丢，理论已空）
                drain_finished_results(ctx, &mut result_rx, &mut answered).await;
                break;
            }
        }
    }

    // 步骤4：消费时机①——一批工具全部完成后、发回 AI 前，经消费门取件
    // （只解禁 guide，规则见 [`ConsumeTiming::AfterToolBatch`]）；批次处理
    // （User 段注入 + Control 执行）；返回值此处不参与决策——工具结果已就绪，
    // 恒回 ReAct 顶部带 guide 消息（若有）+ 工具结果再调 LLM
    let entries = gate.take(ConsumeTiming::AfterToolBatch, &ctx.guide, &ctx.pending);
    consume_batch(ctx, entries).await;
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

/// 采集工作区基线并落一行快照（两类快照触发点共用的执行体）
///
/// 两类触发点：
/// - **工具批边界**（[`handle_tool_calls`] 步骤 2.5）：本批 assistant 消息已落库、
///   工具尚未执行，基线树 = 本批工具执行前的工作区现场
/// - **turn 收尾补拍**（[`track_at_turn_end`]）：锚定 turn 最后一条 assistant 消息 seq，
///   基线树 = 收尾时的工作区状态，`files` = 自本会话最新一行以来的差异 = 最后一批
///   工具的变更窗口
///
/// 落行语义：基线树 = 影子仓 write-tree 结果；变更集 = 对比本会话上一条快照行的
/// 基线树（首拍为空集）；`msg_seq` = 锚点 assistant 消息的落库 seq（行与消息以同一
/// `seq >= target` 谓词同生共死，回退联动据此取行）。
///
/// 降级路径全部 fail-open（WARN、不中断 turn）：
/// - `msg_seq` 为 None：assistant 消息未落库（拦截 Block / 落库失败），无锚点跳过
/// - 快照禁用态（配置关闭 / git 缺失）：零成本跳过
/// - prev_tree 查询 / 采集 / 落行任一失败：本次触发点无快照行，工具照常执行、
///   turn 照常收尾
async fn record_snapshot_row(ctx: &SessionCtx, msg_seq: Option<i64>) {
    // 无锚点不采集：快照行必须关联到一条已落库的 assistant 消息
    let Some(msg_seq) = msg_seq else {
        return;
    };
    // 禁用态零成本跳过（不查 prev_tree、不碰影子仓）
    if !ctx.file_snapshot.is_enabled() {
        return;
    }
    // prev_tree：本会话最新快照行的基线树（None = 首拍，变更集为空）
    let prev_tree = match ctx
        .store
        .latest_file_snapshot_tree(ctx.emitter.session_id())
        .await
    {
        Ok(tree) => tree,
        Err(cause) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                cause = %cause,
                "查询上一快照基线树失败，本次跳过文件快照（不影响工具执行）"
            );
            return;
        }
    };
    // 采集：add -A → write-tree → diff-tree（影子仓内部互斥，同进程多会话串行）
    let outcome = match ctx.file_snapshot.track(prev_tree.as_deref()).await {
        Ok(Some(outcome)) => outcome,
        Ok(None) => return, // 可用态探测与禁用判定间的兜底分支：静默跳过
        Err(cause) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                cause = %cause,
                "文件快照采集失败，本次无快照行（不影响工具执行）"
            );
            return;
        }
    };
    // 落行：失败同样 fail-open（未落上的变更窗口由后续触发点的增量 diff 自然覆盖）
    if let Err(cause) = ctx
        .store
        .insert_file_snapshot(
            ctx.emitter.session_id(),
            msg_seq,
            &outcome.tree_hash,
            &outcome.files,
        )
        .await
    {
        tracing::warn!(
            session_id = ctx.emitter.session_id(),
            cause = %cause,
            "文件快照行落库失败，本次无快照行（不影响工具执行）"
        );
    }
}

/// turn 收尾补拍：承载最后一批工具变更窗口的收尾行
///
/// 快照行的 `files` 语义是「自上一行以来的差异」，由下一次采集落账——turn 的最后
/// 一批工具执行完之后若直接收尾，其文件效果（净新建不删、净修改不复原）无行承载，
/// 回退刚结束的 turn 会漏掉它。turn 结束时本 turn 有过工具批落账则再采一行：
/// - 锚点 = 本 turn 最后一条落库的 assistant 消息 seq（正常完成为最终回复、
///   中断 / 失败退出为最后一批的 assistant 消息），收尾行必为该 turn 末行
/// - 该行 tree = 收尾时工作区状态，只作触碰集载体，永不作回退基线——
///   基线恒取目标后首行，而收尾行之前必有同 turn 的批边界行
/// - 纯对话 turn（无工具批落账）不补拍，零快照成本
///
/// 覆盖全部退出路径：本函数由 [`run_turn`] 在循环体返回后统一调用，正常完成、
/// 中断、shutdown、失败四类退出都经过这里。全链路 fail-open：补拍失败 WARN，
/// 不影响 turn 正常收尾。
async fn track_at_turn_end(ctx: &SessionCtx, ledger: &TurnLedger) {
    if !ledger.tool_batch_tracked {
        return;
    }
    let Some(anchor) = ledger.last_assistant_seq else {
        return;
    };
    record_snapshot_row(ctx, Some(anchor)).await;
}
