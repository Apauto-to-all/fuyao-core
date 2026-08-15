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
//! 中断时保存部分结果（发增量事件），经统一历史入口（`crate::history`）即时落库
//! 每条消息，结束本轮。
//!
//! shutdown：两段 select! 各有 `biased` 优先的 shutdown 分支（优先于 interrupt），
//! 命中后与 interrupt 共用同一条三步链（`handle_stream_interrupt`：
//! emit_interrupt_event → classify → handle_interrupt），把已累积的部分结果经
//! 统一历史入口落库后立即 return。retry.rs 的退避 sleep 也监听 shutdown_token，
//! 收到信号立即冒泡 Cancelled 让本层 shutdown 分支接管。这样 shutdown 不再依赖 10s abort 兜底。

use super::SessionCtx;
use super::builders::{
    ResolvedModel, assistant_payload, build_chat_request, resolve_context_length, resolve_model,
    tool_call_data_to_event, tool_call_event_to_data,
};
use super::handle_control;
use crate::interrupt::{
    SharedTurnState, TurnState, classify, emit_interrupt_event, handle_interrupt,
};
use crate::react::queue;
use crate::stream::StreamResult;
use crate::tool_exec;
use fuyao_api::InterruptSource;
use fuyao_api::TurnDirective;
use fuyao_api::message::EventBase;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::output::{
    AssistantMessage, InterruptMessage as OutputInterruptMessage,
    InterruptPayload as OutputInterruptPayload, TitleMessage, TitlePayload,
};
use fuyao_api::{MessageRole, ModelConfig};
use fuyao_provider::Provider;
use std::sync::Arc;
use tokio::sync::mpsc::Receiver;

/// 构造 shutdown 中断 payload（供两段 select! 的 shutdown 分支复用）
///
/// shutdown 中断语义：source=Shutdown / reason="引擎关闭"。
/// 作为 `handle_stream_interrupt` 的入参，与 interrupt 共用同一条三步链，
/// 让 UI 收到标准 Interrupt 事件，DB 记录能区分「引擎关闭中断」vs「用户主动中断」。
///
/// 内核内部产生的中断载荷本就是 output 侧类型，直接构造。
fn shutdown_interrupt_payload() -> OutputInterruptPayload {
    OutputInterruptPayload::new("引擎关闭", InterruptSource::Shutdown)
}

/// 流式期间中断处理：发通知 + 补增量结果落库（shutdown 与 interrupt 共用同一条三步链）
///
/// 三步链：emit_interrupt_event → classify → handle_interrupt。
/// 调用方负责判定命中哪种信号（shutdown token / interrupt 通道），并把对应的
/// payload 传入——本函数不关心信号来源，只忠实执行"通知 + 补增量"。
///
/// 与工具执行期间的中断处理 `emit_interrupt_and_complete_tool_results` 对仗：
/// 两者覆盖两段 select! 的中断语义，形成"流式 vs 工具执行"一对中断处理器。
///
/// 锁安全沿用 interrupt 模块约定：先 lock 取 TurnState 做 classify，block scope
/// 结束自动释放（不跨 await 持锁）；handle_interrupt 内部再独立 lock + clone。
async fn handle_stream_interrupt(
    ctx: &SessionCtx,
    state: &SharedTurnState,
    payload: &OutputInterruptPayload,
) {
    emit_interrupt_event(payload, &ctx.emitter, &ctx.hooks).await;
    let kind = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        classify(&s)
    };
    handle_interrupt(state, kind, payload, ctx).await;
}

/// run_turn 退出原因——主循环据此决定是否继续消费队列
///
/// 只传决策，不传消息：错误 / 中断的具体内容已通过 `OutputEvent`（Error / Interrupt）
/// 流给上层，本枚举只表达「主循环要不要继续消费 guide/pending」这一决策。
///
/// - `Completed`：双队列跑空、AI 给最终回复。主循环可继续 consume（本就空）或落 select! 等
/// - `HaltedByCommand`：间隙检查点取到 StopTurn 命令（回退 / 手动压缩）。主循环停消费，
///   保留队列剩余，等用户新消息入队触发恢复
/// - `Interrupted`：被用户中断打断。主循环停消费，保留队列剩余
/// - `Failed`：配置错误 / LLM 失败（不可恢复）。主循环停消费
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

/// 运行一轮 ReAct（user messages 已由 run_session 主循环经 inject_messages 注入 DB）
///
/// `model_config` 取自 session 的 SessionParams 快照（决定 model/options），turn 内多轮复用。
/// `rx_interrupt` 为中断通道接收端，两段 select! 监听它。
/// `rx_control` 为控制通道接收端，ReAct loop 顶部间隙检查点消费它——取到任意 StopTurn
/// 命令（手动压缩 / 回退）则立即 return，打断 ReAct 链让命令快速生效（命令自身的 DB
/// 写已在 handle_control 内完成，无需额外落库）。
///
/// 返回 [`TurnOutcome`]：主循环据此决定是否继续消费队列。非 `Completed` 的退出都意味着
/// 「队列剩余不该继续跑」，主循环应跳过 consume 落 select! 等用户新消息恢复。
pub(crate) async fn run_turn(
    ctx: &SessionCtx,
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
            emit_config_error(ctx, &msg).await;
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
            emit_config_error(ctx, &msg).await;
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
        let stream_result = {
            let retry_fut = super::retry::run_stream_with_retry(
                ctx, request, &model, &options, &provider, &state,
            );
            tokio::pin!(retry_fut);
            tokio::select! {
                biased;
                // shutdown 优先（高于 interrupt）：立即落库退出，不等流式结束
                _ = ctx.shutdown_token.cancelled() => {
                    handle_stream_interrupt(ctx, &state, &shutdown_interrupt_payload()).await;
                    return TurnOutcome::Interrupted;
                }
                result = &mut retry_fut => result,
                // 中断通道独立：此处只会收到 Interrupt
                interrupt_msg = rx_interrupt.recv() => {
                    if let Some(interrupt_msg) = interrupt_msg {
                        handle_stream_interrupt(ctx, &state, &interrupt_msg.payload).await;
                        return TurnOutcome::Interrupted;
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
                    handle_final_reply(ctx, &result, &model_config).await;
                    return TurnOutcome::Completed;
                } else {
                    // 有工具调用：发 AssistantMessage → 执行整批工具 → 消费时机①
                    // 返回 true 表示执行期间被 shutdown / interrupt 打断（已落库），需退出 turn
                    let halted = handle_tool_calls(ctx, rx_interrupt, &result, &model_config).await;
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
                let error_event = OutputEvent::Error(fuyao_api::message::output::ErrorMessage {
                    base: EventBase::default(),
                    payload: fuyao_api::message::output::ErrorPayload {
                        message: format!("LLM 调用失败: {e}"),
                        recoverable: false,
                    },
                });
                crate::dispatch::dispatch(&ctx.emitter, &ctx.hooks, error_event).await;
                return TurnOutcome::Failed;
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
    crate::dispatch::dispatch(&ctx.emitter, &ctx.hooks, error_event).await;
}

/// 处理最终回复（AI 不调用工具，一轮 ReAct 结束）
///
/// 固定顺序：① pending 全部倒进 guide ② guide 全部消费注入 messages。
/// 都空 → turn 结束；有 → continue 回 run_turn 顶部再调一轮 LLM。
async fn handle_final_reply(ctx: &SessionCtx, result: &StreamResult, model_config: &ModelConfig) {
    // 回传本轮真实 usage 给主循环（pre-turn 压缩触发判定用）
    *ctx.last_usage.lock().await = Some(result.usage.clone());

    // 经计费入口：拦截 → 事件投影 Message（token/cost/模型归属内化）→ 落 DB → 发送事件
    let event = OutputEvent::Assistant(AssistantMessage {
        base: EventBase::default(),
        payload: assistant_payload(result),
    });
    let _ =
        crate::history::emit_billed_to_history(ctx, event, model_config.model_id.as_str()).await;
    // 拦截 Block：消息不进历史、不计费——插件的责任，引擎不替它兜底

    // 消费时机②：① pending 全倒 guide ② guide 全取注入
    queue::drain_pending_to_guide(&ctx.guide, &ctx.pending);
    let msgs = queue::consume_all_guide(&ctx.guide);
    if msgs.is_empty() {
        // guide 和 pending 都空：turn 结束（消息已在统一历史入口事务内即时落库）
    } else {
        // 有消息：全部注入（每条经统一历史入口拦截→落 DB→发送→观察），回 run_turn 顶部再调一轮 LLM
        crate::history::inject_user_messages(ctx, msgs).await;
    }
}

/// 首轮用户消息后触发标题自动生成（fire-and-forget）
///
/// 在 `inject_messages` 落库首条 user 消息后调用（早于 `run_turn`，不等 AI 回复），
/// 解决旧逻辑「等 AI 整轮回复完成才生成」的延迟硬伤与长回复拖累问题。
///
/// 触发条件（同时满足）：
/// - `[session.title] enabled = true`
/// - 非子 session 或 `[session.title] skip_child = false`：子任务 session 用
///   `parent_session_id` 表达归属，重命名反而扰乱父/子分组与前端过滤
/// - DB 可见消息中 `role=user` 的消息数严格等于 1（首轮判定：计数法比
///   `title=="新会话"` 更稳——用户可能改过 title）
/// - 能取到首条 user content 与当前引擎模型 ID
///
/// 执行模型：`tokio::spawn` 独立 task，不阻塞主 ReAct 循环。
/// spawn 的 future 是 `'static` 的，**不借用 `&mut Session`**——标题直接走
/// `SessionStore::update_title` 单字段 SQL 落库，内存态不更新（下次 resume 时
/// 从 DB 自然读回）。
///
/// 多 session 并发天然安全：clone `Arc<store>` / `Arc<providers>` / `emitter` /
/// `hooks` / `agent_paths` 进 task，各 session task 独立，零共享零协调。
pub(super) async fn maybe_spawn_title_generation(ctx: &SessionCtx, is_child: bool) {
    let title_cfg = &fuyao_api::get_config().session.title;
    if !title_cfg.enabled {
        return;
    }

    // 子 session 跳过（可配置）：parent_session_id 已是归属标记，
    // 默认 skip_child=true 避免重命名扰乱父/子分组
    if is_child && title_cfg.skip_child {
        return;
    }

    // 标题生成在首轮 user 消息落库后触发（仅 1 条消息，不可能压缩过），走从未压缩分支拿到全部消息，
    // keep_tokens 不参与
    let visible = match ctx
        .store
        .load_visible_messages(ctx.emitter.session_id(), 0)
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

    // 计数法判定首轮：user 消息数严格等于 1（后续轮次 user_count 必然 >1，自然跳过）
    let user_count = visible
        .iter()
        .filter(|m| matches!(m.role, MessageRole::User))
        .count();
    if user_count != 1 {
        return;
    }

    // 取首条 user content（标题仅基于用户首句意图）
    let user_content = visible
        .iter()
        .find(|m| matches!(m.role, MessageRole::User))
        .and_then(|m| m.content.clone())
        .unwrap_or_default();

    // 标题生成回退用的主模型 ID：从 session_params 现读模型配置（ReAct 主循环同款快照）。
    // 读不到则空串，maybe_generate_title 内部会因 model_id 无法解析返回 None。
    let main_model_id = {
        let params = ctx.session_params.lock().await;
        params.model_config.model_id.clone()
    };

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
                )
                .await;
            }
            None => tracing::debug!(session_id = %session_id, "标题生成跳过（无可用标题）"),
        }
    });
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
async fn handle_tool_calls(
    ctx: &SessionCtx,
    rx_interrupt: &mut Receiver<OutputInterruptMessage>,
    result: &StreamResult,
    model_config: &ModelConfig,
) -> bool {
    // 步骤1：逐个拦截 ToolCall 事件，构造 effective_tool_calls
    // 整批 tool_calls 拆成单个 ToolCall 事件各自拦截；Block 的跳过。
    let mut effective_tool_calls: Vec<fuyao_provider::ToolCallData> =
        Vec::with_capacity(result.tool_calls.len());
    for tc in &result.tool_calls {
        let event = tool_call_data_to_event(tc);
        if let Some(intercepted) = crate::dispatch::intercept(&ctx.emitter, &ctx.hooks, event).await
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
    let _ =
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
    // 中断/shutdown 时 channel 里已完成的也落库（不丢），未完成的补发中断式 ToolResult。
    //
    // shutdown 与 interrupt 的落库逻辑完全相同（发 Interrupt 通知 → 补未完成 tool_result），
    // 抽 `emit_interrupt_and_complete_tool_results` 复用，唯一差异是 payload 的 source/reason。
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

    loop {
        tokio::select! {
            biased; // shutdown / 中断优先，保证及时响应
            // shutdown 优先（高于 interrupt）：立即清空已完成工具结果 + 落库退出
            // 工具执行 fut 被 drop → JoinSet drop → tokio 自动 abort 所有未完成工具 task
            _ = ctx.shutdown_token.cancelled() => {
                // 清空 channel 把已完成的落库（不丢已完成结果）
                while let Ok(r) = result_rx.try_recv() {
                    push_tool_result_to_history(ctx, r).await;
                }
                emit_interrupt_and_complete_tool_results(
                    ctx, &effective_result.tool_calls,
                    &shutdown_interrupt_payload(),
                ).await;
                return true;
            }
            cmd = rx_interrupt.recv() => {
                // interrupt 命中：显式 cancel 工具批 child_token（shutdown 靠 parent 传播，无需此处 cancel）
                // 让监听 token 的长任务 handler 后台优雅收尾；无宽限期，立即清空 + 补发
                cancel.cancel();
                // 收到 Interrupt 或通道关闭（None）：清空 channel 把已完成的落库
                // 用 try_recv 非阻塞清空（exec_fut 可能还在跑，recv 会阻塞）
                while let Ok(r) = result_rx.try_recv() {
                    push_tool_result_to_history(ctx, r).await;
                }
                if let Some(ref interrupt_msg) = cmd {
                    emit_interrupt_and_complete_tool_results(
                        ctx, &effective_result.tool_calls,
                        &interrupt_msg.payload,
                    ).await;
                }
                return true;
            }
            Some(r) = result_rx.recv() => {
                // 完成一个：立即走统一历史入口（拦截 → 落 DB → 发送事件）
                push_tool_result_to_history(ctx, r).await;
            }
            _ = &mut exec_fut => {
                // execute_tools 完成：清空 channel 里剩余的（防丢，理论已空）
                while let Ok(r) = result_rx.try_recv() {
                    push_tool_result_to_history(ctx, r).await;
                }
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

/// 把工具执行结果经统一入口单条落 DB
///
/// 工具完成时立即调用：拦截 → 投影 Message（含 tool_name）→ 落 DB → 发送事件 → 观察。
/// 保证「拦截→存储→发送」三者一致；中断时已完成的也不丢。
async fn push_tool_result_to_history(ctx: &SessionCtx, result: tool_exec::ToolExecResult) {
    let event = OutputEvent::ToolResult(fuyao_api::message::output::ToolResultMessage {
        base: EventBase::default(),
        payload: fuyao_api::message::output::ToolResultPayload {
            tool_call_id: result.tool_call_id,
            tool_name: result.tool_name,
            content: result.content,
        },
    });
    let _ = crate::history::emit_to_history(ctx, event).await;
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
    effective_tool_calls: &[fuyao_provider::ToolCallData],
    payload: &OutputInterruptPayload,
) {
    emit_interrupt_event(payload, &ctx.emitter, &ctx.hooks).await;

    // 从 DB 查询已落库的 answered tool_call_id（事件级落库模式下消息不在内存）
    // keep_tokens 与主对话同口径（effective_keep_tokens 按 context_length 算）。
    // 本路径在 run_turn 之外、无 resolved 在手：复用 resolve_context_length 纯函数，
    // 传入 session 的 model_id（创建会话时已给定的非空值）；模型未注册时为 None，
    // 保留预算按 0（与主对话同语义）。
    let context_length = {
        let p = ctx.session_params.lock().await;
        let model_id = p.model_config.model_id.as_str();
        resolve_context_length(model_id, &ctx.agent_paths)
    };
    let keep_tokens = ctx
        .compression_config
        .effective_keep_tokens(context_length.unwrap_or(0));
    let answered: std::collections::HashSet<String> = match ctx
        .store
        .load_visible_messages(ctx.emitter.session_id(), keep_tokens)
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

    // 为 effective 中未完成的 tool_call 补发中断式 ToolResult（也走统一落库入口）
    for tc in effective_tool_calls {
        if !answered.contains(&tc.id) {
            let ev = crate::interrupt::make_interrupt_tool_result(
                tc.id.clone(),
                tc.name.clone(),
                &payload.source,
                &payload.reason,
            );
            let _ = crate::history::emit_to_history(ctx, ev).await;
        }
    }
}
