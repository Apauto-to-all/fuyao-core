//! 消息处理管道（dispatch）
//!
//! 统一所有输出消息的处理链路：拦截 → 发送 → 观察。
//! 管道在每个 session 内运行（session 层），引擎层只负责装配 hooks
//! 并路由消息到对应 session 的管道。多 session 各自一条独立管道，互不干扰。
//!
//! 三段职责（串行执行）：
//! 1. **拦截（intercept）**：插件经 `&mut` 原地修改事件或返回原因阻止
//!    （阻止即整条丢弃）
//! 2. **发送（deliver）**：经 `Emitter::emit` 推到出口通道（全引擎唯一发送出口）
//! 3. **观察（observe）**：插件只读副作用（持久化/日志/统计），
//!    多钩子经 `Arc` 共享同一份事件本体，不随钩子数量复制
//!
//! 本模块只提供管道原语，不感知「进历史」语义：
//! - 不进历史的纯事件（Chunk/Error/Compression/Interrupt 通知等）用 [`dispatch`]
//! - 进历史的消息（要落 DB、参与计费）统一走 [`crate::history`] 的入口——
//!   那里在管道之上叠加「事件投影 Message 落库 + seq 回填 + 计费」
//! - 工具调用需要拿拦截结果回灌时，用 [`intercept`] 单独拦截

use crate::emit::Emitter;
use fuyao_api::message::OutputEvent;
use fuyao_hooks::SharedHooks;
use std::sync::Arc;

// ===== 管道各段：intercept / deliver =====

/// 执行拦截钩子
///
/// 拿 owned 事件原地过拦截链：钩子经 `&mut` 直接改写事件（原地修改零分配）。
/// 返回 `Some(event)` 表示通过（事件可能已被插件修改）；返回 `None` 表示
/// 某钩子阻止（携带原因，已记 WARN 日志），调用方应丢弃该事件。
///
/// registry 装配后只读（无锁共享），拦截是同步调用——本函数因而不是 async，
/// 调用方无需 await（热路径省一次状态机开销）。
pub(crate) fn intercept(hooks: &SharedHooks, event: OutputEvent) -> Option<OutputEvent> {
    let mut ev = event;
    match hooks.hook_output_intercept(&mut ev) {
        // None=全部通过：原地修改后的事件还给调用方继续走管道
        None => Some(ev),
        // Some(reason)=阻止：丢弃整条
        Some(reason) => {
            tracing::warn!(
                hook = "output_intercept",
                block = true,
                reason = %reason,
                "事件被拦截钩子丢弃"
            );
            None
        }
    }
}

/// 发送事件到出口通道 + 触发观察钩子
///
/// 顺序：先 `Emitter::emit`（盖 session_id + tx.send），后 `hook_output_observe`。
///
/// 事件物化成本恒定为一次 clone：本处为 emit clone 一份（emit 按值消费并盖
/// session_id 标签），原事件本体包进 `Arc` 给观察侧——任意多个观察钩子共享
/// 同一份 Arc（引用计数拷贝），成本不随钩子数量增长。
///
/// observe 钩子按优先级序（同优先级按注册序）串行执行，单个 panic 或超时
/// 不阻塞后续（见 HooksRegistry）。观察侧看到的是 emit 盖标签前的事件本体，
/// 插件随 session 装配运行，session 身份由插件自持。
///
/// 暴露为 `pub(crate)` 供工具调用等分离式场景在拦截 + 处理后单独调用；
/// 事件按值消费，调用方还需要事件本体时须在调用前自行留存。
pub(crate) async fn deliver(emitter: &Emitter, hooks: &SharedHooks, event: OutputEvent) {
    // 观察侧共享本体：原事件包 Arc（零拷贝），所有观察钩子共享这一份
    let observe_event = Arc::new(event);

    // 先发送（Emitter 负责：盖 session_id 标签 + 推到出口通道）
    // 出站通道无界，emit 同步返回——但本函数保留 async 因 observe hook 跨 await
    emitter.emit(observe_event.as_ref().clone());

    // 再观察
    hooks.hook_output_observe(observe_event).await;
}

// ===== 完整管道入口 =====

/// 完整管道：拦截 → 发送 → 观察
///
/// Block 时整条丢弃（不发送、不观察）。
///
/// 注意：本函数**不落 DB**——只走管道。如需把拦截后的消息落到历史
/// （进 DB + 下轮 LLM 输入），用 [`crate::history::emit_to_history`]。
pub(crate) async fn dispatch(emitter: &Emitter, hooks: &SharedHooks, event: OutputEvent) {
    // 1. 拦截（同步原地修改）
    let Some(intercepted) = intercept(hooks, event) else {
        return; // Block：丢弃（不发送、不观察）
    };

    // 2. 发送 + 3. 观察
    deliver(emitter, hooks, intercepted).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::message::output::{AssistantMessage, AssistantPayload};
    use fuyao_hooks::HooksRegistry;
    use tokio::sync::mpsc;

    /// 构造测试用 Emitter + 出站接收端
    fn make_emitter() -> (Emitter, mpsc::UnboundedReceiver<OutputEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let emitter = Emitter::new(tx, "test-session".to_string());
        (emitter, rx)
    }

    /// 把已注册好钩子的 registry 冻结包 Arc（复刻装配期语义：register → finalize → 共享）
    fn freeze_hooks(registry: HooksRegistry) -> SharedHooks {
        Arc::new(registry)
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
        let (emitter, mut rx) = make_emitter();
        let hooks = freeze_hooks(HooksRegistry::default());
        dispatch(&emitter, &hooks, make_assistant_event("hello")).await;

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
    async fn intercept_returns_modified_event() {
        // 拦截器原地修改 content：intercept 返回修改后的事件
        let mut reg = HooksRegistry::default();
        reg.register_output_intercept(
            0,
            Arc::new(|ev: &mut OutputEvent| {
                if let OutputEvent::Assistant(m) = ev
                    && let Some(c) = &mut m.payload.content
                {
                    *c = c.to_uppercase();
                }
                None
            }),
        );
        let hooks = freeze_hooks(reg);

        let result = intercept(&hooks, make_assistant_event("hi"));
        match result {
            Some(OutputEvent::Assistant(m)) => {
                assert_eq!(m.payload.content.as_deref(), Some("HI"));
            }
            _ => panic!("拦截通过应返回修改后的事件"),
        }
    }

    #[tokio::test]
    async fn intercept_returns_none_on_block() {
        // 拦截器返回阻止原因：返回 None，事件被丢弃
        let mut reg = HooksRegistry::default();
        reg.register_output_intercept(
            0,
            Arc::new(|_: &mut OutputEvent| Some("插件拦截".to_string())),
        );
        let hooks = freeze_hooks(reg);

        let result = intercept(&hooks, make_assistant_event("hi"));
        assert!(result.is_none(), "阻止应返回 None");
    }

    #[tokio::test]
    async fn dispatch_drops_event_on_block() {
        // Block 时 dispatch 整条丢弃，不发送、不观察
        let (emitter, mut rx) = make_emitter();
        let mut reg = HooksRegistry::default();
        reg.register_output_intercept(
            0,
            Arc::new(|_: &mut OutputEvent| Some("拦截丢弃".to_string())),
        );
        let hooks = freeze_hooks(reg);
        dispatch(&emitter, &hooks, make_assistant_event("dropped")).await;

        assert!(rx.try_recv().is_err(), "Block 后不应有事件发出");
    }

    #[tokio::test]
    async fn deliver_runs_observe_hooks() {
        // deliver 之后 observe 被调用
        let (emitter, mut rx) = make_emitter();
        let observed = Arc::new(std::sync::Mutex::new(false));
        let mut reg = HooksRegistry::default();
        {
            let observed = observed.clone();
            reg.register_output_observe(
                0,
                Arc::new(move |_ev: Arc<OutputEvent>| {
                    Box::pin({
                        let observed = observed.clone();
                        async move {
                            *observed.lock().unwrap() = true;
                        }
                    })
                }),
            );
        }
        let hooks = freeze_hooks(reg);

        deliver(&emitter, &hooks, make_assistant_event("observed")).await;
        assert!(rx.try_recv().is_ok(), "deliver 应发出事件");
        assert!(*observed.lock().unwrap(), "observe 钩子应被调用");
    }

    #[tokio::test]
    async fn deliver_stamps_session_id() {
        // 经管道发送的事件带 session_id 标签
        let (emitter, mut rx) = make_emitter();
        let hooks = freeze_hooks(HooksRegistry::default());
        deliver(&emitter, &hooks, make_assistant_event("tagged")).await;
        let received = rx.recv().await.expect("应收到事件");
        match received {
            OutputEvent::Assistant(m) => {
                assert_eq!(m.base.session_id.as_deref(), Some("test-session"));
            }
            _ => panic!("应为 Assistant 事件"),
        }
    }
}
