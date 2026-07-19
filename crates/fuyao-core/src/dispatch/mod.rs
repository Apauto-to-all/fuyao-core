//! 消息处理管道（dispatch）
//!
//! 统一所有输出消息的处理链路：拦截 → 处理 → 发送 → 观察。
//! 管道在每个 session 内运行（session 层），引擎层只负责装配 hooks
//! 并路由消息到对应 session 的管道。多 session 各自一条独立管道，互不干扰。
//!
//! 四段职责（串行执行）：
//! 1. **拦截（intercept）**：插件可修改或阻断事件（`InterceptResult::Block` 短路丢弃）
//! 2. **处理（process）**：引擎内部业务逻辑，由调用方按消息类型注入回调。
//!    回调必须是轻量、不阻塞的动作（入队、收集批次、投递信号）；
//!    耗时的动作（如工具执行）在管道外批量进行。
//! 3. **发送（deliver）**：经 `Emitter::emit` 推到出口通道（全引擎唯一发送出口）
//! 4. **观察（observe）**：插件只读副作用（持久化/日志/统计）
//!
//! 调用方式：
//! - **进历史的消息**（User 回显后的入队、Assistant、ToolResult 等）用
//!   [`emit_to_history`]：拦截 → 用拦截后事件构造 Message 并 push 进 session.messages
//!   → 发送事件 → 观察。**所有要落到 DB 的消息必经此入口**，保证拦截→存储→消费三者一致
//! - 不进历史的纯事件（Chunk/Error/Compression/Interrupt 通知等）用 [`dispatch`]，process 传 None
//! - 工具调用需要拿拦截结果回灌时，用 [`dispatch_intercept`] 单独拦截

mod deliver;
mod intercept;

use crate::emit::Emitter;
use fuyao_api::message::OutputEvent;
use fuyao_api::{Message, Session};
use fuyao_hooks::SharedHooks;
use std::future::Future;
use std::pin::Pin;

pub(crate) use deliver::deliver;
pub(crate) use intercept::intercept;

/// 处理回调类型（拦截后、发送前执行的不阻塞动作）
///
/// 拿到拦截后的消息（只读借用），执行轻量副作用（入队、收集批次、投递信号）。
/// 耗时动作（工具执行）不放在这里——它们在管道外批量进行。
pub(crate) type ProcessFn =
    Box<dyn FnOnce(&OutputEvent) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

/// 完整管道：拦截 → 处理 → 发送 → 观察
///
/// `process` 为处理回调：拦截 Pass 后、发送前执行。传 `None` 表示无处理动作（透传）。
/// 处理回调必须是轻量、不阻塞的（入队、收集、投递）；耗时动作（工具执行）在管道外。
///
/// Block 时整条丢弃（不执行处理、不发送、不观察）。
///
/// 注意：本函数**不 push session.messages**——只走管道。如需把拦截后的消息落到
/// 历史（进 DB + 下轮 LLM 输入），用 [`emit_to_history`]。
pub(crate) async fn dispatch(
    emitter: &Emitter,
    hooks: &SharedHooks,
    event: OutputEvent,
    process: Option<ProcessFn>,
) {
    // 1. 拦截
    let Some(intercepted) = intercept(emitter, hooks, event).await else {
        return; // Block：丢弃
    };

    // 2. 处理（拦截后、发送前；轻量不阻塞的动作）
    if let Some(process_fn) = process {
        process_fn(&intercepted).await;
    }

    // 3. 发送 + 4. 观察
    deliver(emitter, hooks, intercepted).await;
}

/// 进历史消息的统一出口：拦截 → 构造 Message push → 发送事件 → 观察
///
/// 所有要进 `session.messages`（影响下轮 LLM 输入 + 落库）的消息必经此入口。
/// 保证「拦截 → 存储 → 消费」三者数据一致——拦截后的事件既用来构造 Message push，
/// 又用来发送给 UI，同源不分裂。
///
/// `msg_from_event` 闭包从拦截后的 OutputEvent 提取字段构造 Message。
/// 返回 `None` 表示该事件不应进历史（如转换失败或不匹配的事件类型）。
///
/// **token + cost 自动累积**：闭包返回的 Message 应已填好 token + cost 字段
/// （由调用方在闭包内用 `fill_assistant_message_usage_and_cost` 填充）。
/// 本函数识别 assistant 角色自动累积 `session.total_*`——push 和计费强绑定，
/// 未来新增 assistant 产出点不会漏算 cost。
///
/// Block 时：不 push、不发，返回 None（调用方据此跳过后续动作，如入队）。
/// 与现有 ToolCall Block 语义一致——消息不进历史、UI 看不到，是插件的责任。
///
/// 返回拦截后事件供调用方做后续动作（如入队、计费）。
pub(crate) async fn emit_to_history(
    emitter: &Emitter,
    hooks: &SharedHooks,
    session: &mut Session,
    event: OutputEvent,
    msg_from_event: impl FnOnce(&OutputEvent) -> Option<Message>,
) -> Option<OutputEvent> {
    // 1. 拦截
    let intercepted = intercept(emitter, hooks, event).await?;

    // 2. 用拦截后事件构造 Message，识别 assistant 累积 session 总计后 push
    if let Some(msg) = msg_from_event(&intercepted) {
        // 自动累积 session.total_*（仅 assistant 角色；cost 用 Decimal 精确累加，
        // 避免 f64 加法误差——累积逻辑统一在 fuyao_session::accumulate_session_total）
        let is_assistant = msg.role == "assistant";
        let msg_cost = msg.cost;
        fuyao_session::accumulate_session_total(session, &msg);
        if is_assistant {
            tracing::debug!(
                session_id = emitter.session_id(),
                cost = msg_cost,
                total_cost = session.total_cost,
                "本轮费用已累积"
            );
        }
        session.messages.push(msg);
    }

    // 3. 发送事件给 UI + 4. 观察钩子
    deliver(emitter, hooks, intercepted.clone()).await;

    Some(intercepted)
}

/// 仅拦截（不含处理/发送/观察），返回拦截后事件
///
/// 用于 User 入站等需要拿拦截后 payload 做后续动作（入队用拦截后 content）
/// 但不直接 push session.messages 的场景（push 时机由队列消费决定）。
pub(crate) async fn dispatch_intercept(
    emitter: &Emitter,
    hooks: &SharedHooks,
    event: OutputEvent,
) -> Option<OutputEvent> {
    intercept(emitter, hooks, event).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::message::output::{AssistantMessage, AssistantPayload};
    use fuyao_hooks::{HooksRegistry, InterceptResult};
    use tokio::sync::mpsc;

    /// 构造测试用 Emitter + 空 hooks
    fn make_emitter_hooks() -> (Emitter, SharedHooks, mpsc::Receiver<OutputEvent>) {
        let (tx, rx) = mpsc::channel(16);
        let emitter = Emitter::new(tx, "test-session".to_string());
        let hooks: SharedHooks =
            std::sync::Arc::new(tokio::sync::Mutex::new(HooksRegistry::default()));
        (emitter, hooks, rx)
    }

    /// 构造一个简单的 Assistant 事件
    fn make_assistant_event(content: &str) -> OutputEvent {
        OutputEvent::Assistant(AssistantMessage {
            base: fuyao_api::message::EventBase::default(),
            payload: AssistantPayload {
                content: Some(content.to_string()),
                reasoning: None,
                tool_calls: None,
                finish_reason: Some("stop".to_string()),
                completion_tokens: 0,
                prompt_tokens: 0,
                total_tokens: 0,
                reasoning_tokens: 0,
                cached_tokens: 0,
            },
        })
    }

    #[tokio::test]
    async fn dispatch_passes_event_through_when_no_hooks() {
        // 空 hooks、无处理：事件直通，接收端能收到
        let (emitter, hooks, mut rx) = make_emitter_hooks();
        dispatch(&emitter, &hooks, make_assistant_event("hello"), None).await;

        let received = rx.recv().await.expect("应收到事件");
        match received {
            OutputEvent::Assistant(m) => {
                assert_eq!(m.payload.content.as_deref(), Some("hello"));
                assert_eq!(m.base.session_id.as_deref(), Some("test-session"));
            }
            _ => panic!("应为 Assistant 事件"),
        }
    }

    #[tokio::test]
    async fn dispatch_intercept_returns_modified_event() {
        // 拦截器修改 content：dispatch_intercept 返回修改后的事件
        let (emitter, hooks, _rx) = make_emitter_hooks();
        {
            let mut reg = hooks.lock().await;
            reg.register_output_intercept(
                0,
                std::sync::Arc::new(|ev| {
                    if let OutputEvent::Assistant(m) = ev {
                        let mut modified = m.clone();
                        if let Some(c) = &mut modified.payload.content {
                            *c = c.to_uppercase();
                        }
                        InterceptResult::Pass(OutputEvent::Assistant(modified))
                    } else {
                        InterceptResult::Pass(ev.clone())
                    }
                }),
            );
        }

        let result = dispatch_intercept(&emitter, &hooks, make_assistant_event("hi")).await;
        match result {
            Some(OutputEvent::Assistant(m)) => {
                assert_eq!(m.payload.content.as_deref(), Some("HI"));
            }
            _ => panic!("拦截 Pass 应返回修改后的事件"),
        }
    }

    #[tokio::test]
    async fn dispatch_intercept_returns_none_on_block() {
        // 拦截器 Block：返回 None，事件被丢弃
        let (emitter, hooks, _rx) = make_emitter_hooks();
        {
            let mut reg = hooks.lock().await;
            reg.register_output_intercept(
                0,
                std::sync::Arc::new(|_| InterceptResult::Block("插件拦截".to_string())),
            );
        }

        let result = dispatch_intercept(&emitter, &hooks, make_assistant_event("hi")).await;
        assert!(result.is_none(), "Block 应返回 None");
    }

    #[tokio::test]
    async fn dispatch_drops_event_on_block() {
        // Block 时 dispatch 整条丢弃，不执行处理、不发送
        let (emitter, hooks, mut rx) = make_emitter_hooks();
        let processed = std::sync::Arc::new(std::sync::Mutex::new(false));
        {
            let mut reg = hooks.lock().await;
            reg.register_output_intercept(
                0,
                std::sync::Arc::new(|_| InterceptResult::Block("拦截丢弃".to_string())),
            );
        }
        let processed_clone = processed.clone();
        let process_fn: ProcessFn = Box::new(move |_| {
            Box::pin(async move {
                *processed_clone.lock().unwrap() = true;
            })
        });
        dispatch(
            &emitter,
            &hooks,
            make_assistant_event("dropped"),
            Some(process_fn),
        )
        .await;

        assert!(rx.try_recv().is_err(), "Block 后不应有事件发出");
        assert!(!*processed.lock().unwrap(), "Block 时处理回调不应执行");
    }

    #[tokio::test]
    async fn dispatch_runs_process_before_deliver() {
        // process 在 deliver 之前执行：先标记 processed，再发出
        let (emitter, hooks, mut rx) = make_emitter_hooks();
        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let order_clone = order.clone();
        let process_fn: ProcessFn = Box::new(move |_| {
            let order = order_clone.clone();
            Box::pin(async move {
                order.lock().unwrap().push("process");
            })
        });
        // observe 也标记，验证 process 在 observe 前
        {
            let order = order.clone();
            let mut reg = hooks.lock().await;
            reg.register_output_observe(std::sync::Arc::new(move |_| {
                let order = order.clone();
                Box::pin(async move {
                    order.lock().unwrap().push("observe");
                })
            }));
        }

        dispatch(
            &emitter,
            &hooks,
            make_assistant_event("test"),
            Some(process_fn),
        )
        .await;
        assert!(rx.try_recv().is_ok(), "应发出事件");

        let order = order.lock().unwrap();
        assert_eq!(
            *order,
            vec!["process", "observe"],
            "顺序应为 process → observe"
        );
    }

    #[tokio::test]
    async fn deliver_runs_observe_hooks() {
        // deliver 之后 observe 被调用
        let (emitter, hooks, mut rx) = make_emitter_hooks();
        let observed = std::sync::Arc::new(std::sync::Mutex::new(false));
        {
            let observed = observed.clone();
            let mut reg = hooks.lock().await;
            reg.register_output_observe(std::sync::Arc::new(move |_ev| {
                Box::pin({
                    let observed = observed.clone();
                    async move {
                        *observed.lock().unwrap() = true;
                    }
                })
            }));
        }

        deliver(&emitter, &hooks, make_assistant_event("observed")).await;
        assert!(rx.try_recv().is_ok(), "deliver 应发出事件");
        assert!(*observed.lock().unwrap(), "observe 钩子应被调用");
    }

    #[tokio::test]
    async fn deliver_stamps_session_id() {
        // 经管道发送的事件带 session_id 标签
        let (emitter, hooks, mut rx) = make_emitter_hooks();
        deliver(&emitter, &hooks, make_assistant_event("tagged")).await;
        let received = rx.recv().await.expect("应收到事件");
        match received {
            OutputEvent::Assistant(m) => {
                assert_eq!(m.base.session_id.as_deref(), Some("test-session"));
            }
            _ => panic!("应为 Assistant 事件"),
        }
    }

    // ===== emit_to_history 测试 =====

    /// 从 Assistant 事件构造 Message（模拟业务闭包）
    fn assistant_msg_from_event(ev: &OutputEvent) -> Option<Message> {
        match ev {
            OutputEvent::Assistant(m) => {
                let mut msg = Message::assistant(m.payload.content.clone());
                msg.reasoning = m.payload.reasoning.clone();
                Some(msg)
            }
            _ => None,
        }
    }

    #[tokio::test]
    async fn emit_to_history_pushes_message_when_pass() {
        // 拦截 Pass：用拦截后 payload 构造 Message push 进 session.messages，事件也发出
        let (emitter, hooks, mut rx) = make_emitter_hooks();
        let mut session = Session::default();

        emit_to_history(
            &emitter,
            &hooks,
            &mut session,
            make_assistant_event("hello"),
            assistant_msg_from_event,
        )
        .await;

        // session.messages 应有一条 Message，content 为 "hello"
        assert_eq!(session.messages.len(), 1, "Pass 时应 push 一条 Message");
        assert_eq!(session.messages[0].content.as_deref(), Some("hello"));
        assert_eq!(session.messages[0].role, "assistant");

        // 事件也应发出
        assert!(rx.try_recv().is_ok(), "Pass 时应发出事件");
    }

    #[tokio::test]
    async fn emit_to_history_pushes_modified_content() {
        // 拦截修改 content 后，session.messages 里的 Message 携带修改后内容
        // （拦截→存储→消费三者一致的核心验证）
        let (emitter, hooks, _rx) = make_emitter_hooks();
        {
            let mut reg = hooks.lock().await;
            reg.register_output_intercept(
                0,
                std::sync::Arc::new(|ev| {
                    if let OutputEvent::Assistant(m) = ev {
                        let mut modified = m.clone();
                        if let Some(c) = &mut modified.payload.content {
                            *c = format!("[已脱敏]{c}");
                        }
                        InterceptResult::Pass(OutputEvent::Assistant(modified))
                    } else {
                        InterceptResult::Pass(ev.clone())
                    }
                }),
            );
        }
        let mut session = Session::default();

        emit_to_history(
            &emitter,
            &hooks,
            &mut session,
            make_assistant_event("secret"),
            assistant_msg_from_event,
        )
        .await;

        // 关键断言：session.messages 里的是修改后内容（与 UI 看到的一致）
        assert_eq!(session.messages.len(), 1);
        assert_eq!(
            session.messages[0].content.as_deref(),
            Some("[已脱敏]secret"),
            "session.messages 应携带拦截后的内容"
        );
    }

    #[tokio::test]
    async fn emit_to_history_skips_on_block() {
        // 拦截 Block：不 push、不发，返回 None
        let (emitter, hooks, mut rx) = make_emitter_hooks();
        {
            let mut reg = hooks.lock().await;
            reg.register_output_intercept(
                0,
                std::sync::Arc::new(|_| InterceptResult::Block("插件拦截".to_string())),
            );
        }
        let mut session = Session::default();

        let result = emit_to_history(
            &emitter,
            &hooks,
            &mut session,
            make_assistant_event("blocked"),
            assistant_msg_from_event,
        )
        .await;

        assert!(result.is_none(), "Block 应返回 None");
        assert!(session.messages.is_empty(), "Block 时不应 push Message");
        assert!(rx.try_recv().is_err(), "Block 时不应发出事件");
    }
}
