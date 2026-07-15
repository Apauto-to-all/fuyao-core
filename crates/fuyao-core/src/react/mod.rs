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

use crate::emit::Emitter;
use crate::engine::types::SharedQueue;
use crate::interrupt::emit_interrupt_event;
use crate::tool_registry::ToolRegistry;
use fuyao_api::Session;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::input::InterruptMessage;
use fuyao_hooks::SharedHooks;
use fuyao_provider::Provider;
use std::sync::Arc;
use tokio::sync::Notify;
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
    pub emitter: Emitter,
    /// 引导队列（直接消费）
    pub guide: SharedQueue,
    /// 排队队列（最终回复后转入 guide）
    pub pending: SharedQueue,
}

/// session 的独立执行流
///
/// 消费 guide 队列驱动 ReAct 循环；guide 空时 select! 等待 notify（新消息入队）
/// 或 rx_interrupt（idle 中断）。pending 入队也会 notify（覆盖 AI 空闲只发 Pending）。
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
    notify: Arc<Notify>,
    mut rx_interrupt: Receiver<InterruptMessage>,
    mut session: Session,
    store: Arc<fuyao_session::SessionStore>,
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    hooks: SharedHooks,
    agent_paths: fuyao_api::AgentPaths,
    tx_event: Sender<OutputEvent>,
) {
    tracing::info!(session_id = %session_id, "session 执行流启动");

    let ctx = SessionCtx {
        store,
        provider,
        tools,
        hooks,
        agent_paths,
        emitter: Emitter::new(tx_event, session_id.clone()),
        guide,
        pending,
    };

    // 主循环：从 guide 全取消息 → 注入 → 跑一轮 ReAct；guide 空 → 等待
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
            // 取第一条消息的 params（决定本轮 model/options）
            // ReAct 多轮复用同一份 model（一个 turn 一个模型）
            // TODO: 多条 guide 消息 params 不一致时如何取——当前取第一条
            let first_params = msgs.first().map(|m| m.params.clone()).unwrap_or_default();
            // 一次性全部注入：每条变一条 user message
            queue::inject_messages(&ctx.emitter, &ctx.hooks, &mut session, msgs).await;
            turn::run_turn(&ctx, &mut session, &mut rx_interrupt, first_params).await;
        } else {
            // guide 空：等 notify（新消息入队）或中断
            tokio::select! {
                // notify 唤醒：回循环顶部重新 consume（guide 或 pending 可能有新消息）
                () = notify.notified() => { continue; }
                Some(interrupt_msg) = rx_interrupt.recv() => {
                    // idle 中断：无活跃 turn，只发通知事件
                    emit_interrupt_event(&interrupt_msg.payload, &ctx.emitter, &ctx.hooks).await;
                    tracing::debug!(session_id = %ctx.emitter.session_id(), "idle 时收到中断信号");
                }
            }
        }
    }
}
