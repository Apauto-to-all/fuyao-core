//! Hooks 注册表与执行引擎
//!
//! 拦截钩子（异步串行，可取消带原因，panic 防护）+ 观察钩子（异步串行）。
//! 按优先级排序执行。

use crate::plugin::SessionSender;
use crate::plugin::panic_payload_to_string;
use crate::types::*;
use futures_util::future::FutureExt;
use fuyao_api::message::OutputEvent;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

/// 带优先级的钩子条目
struct Prioritized<T> {
    priority: i32,
    handler: T,
}

/// Hooks 注册表
pub struct HooksRegistry {
    /// 输出拦截钩子列表
    output_intercept: Vec<Prioritized<OutputInterceptFn>>,
    /// 输出观察钩子列表
    output_observe: Vec<Prioritized<OutputObserveFn>>,
    /// 发送输入事件钩子列表（插件发送任意 InputEvent，引擎统一入队）
    send_input: Vec<Prioritized<SendInputFn>>,
    /// 标记是否需要排序
    dirty: bool,
    /// 单个 hook 执行超时；零值表示不超时
    hook_timeout: Duration,
}

impl Default for HooksRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HooksRegistry {
    pub fn new() -> Self {
        let timeout_secs = fuyao_api::get_config().hooks.timeout_secs;
        Self {
            output_intercept: Vec::new(),
            output_observe: Vec::new(),
            send_input: Vec::new(),
            dirty: false,
            hook_timeout: Duration::from_secs(timeout_secs),
        }
    }

    pub fn register_output_intercept(&mut self, priority: i32, handler: OutputInterceptFn) {
        self.output_intercept
            .push(Prioritized { priority, handler });
        self.dirty = true;
    }

    pub fn register_output_observe(&mut self, handler: OutputObserveFn) {
        self.output_observe.push(Prioritized {
            priority: 0,
            handler,
        });
    }

    /// 注册发送输入事件钩子
    pub fn register_send_input(&mut self, priority: i32, handler: SendInputFn) {
        self.send_input.push(Prioritized { priority, handler });
        self.dirty = true;
    }

    /// 按 priority 降序排序（高优先级先执行）
    fn ensure_sorted(&mut self) {
        if !self.dirty {
            return;
        }
        fn sort_by_priority<T>(v: &mut [Prioritized<T>]) {
            v.sort_by_key(|b| std::cmp::Reverse(b.priority));
        }
        sort_by_priority(&mut self.output_intercept);
        sort_by_priority(&mut self.send_input);
        self.dirty = false;
    }

    /// 带超时执行 hook future。
    ///
    /// - `hook_timeout` 为零：不超时，直接 await
    /// - `hook_timeout` > 0：用 `tokio::time::timeout` 包裹，超时返回 None
    ///
    /// 调用方负责先做 `catch_unwind`（panic 防护），本方法只管超时。
    async fn run_hook_with_timeout<T>(&self, fut: impl Future<Output = T>) -> Option<T> {
        if self.hook_timeout.is_zero() {
            Some(fut.await)
        } else {
            tokio::time::timeout(self.hook_timeout, fut).await.ok()
        }
    }

    /// 执行输出拦截钩子：串行，panic 防护，任一返回 Block 则立即返回
    pub fn hook_output_intercept(&self, msg: &OutputEvent) -> InterceptResult<OutputEvent> {
        let mut current = msg.clone();
        for entry in &self.output_intercept {
            match std::panic::catch_unwind(AssertUnwindSafe(|| (entry.handler)(&current))) {
                Ok(InterceptResult::Pass(modified)) => current = modified,
                Ok(InterceptResult::Block(reason)) => return InterceptResult::Block(reason),
                Err(payload) => {
                    tracing::warn!(
                        hook = "output_intercept",
                        recovered = true,
                        cause = %panic_payload_to_string(&*payload),
                        "钩子执行 panic 已恢复"
                    );
                }
            }
        }
        InterceptResult::Pass(current)
    }

    /// 执行输出观察钩子：串行，panic 防护
    ///
    /// 按注册顺序逐个执行，单个钩子 panic 不阻塞后续钩子。
    pub async fn hook_output_observe(&self, msg: OutputEvent) {
        for entry in &self.output_observe {
            let handler_fut = AssertUnwindSafe((entry.handler)(msg.clone())).catch_unwind();
            match self.run_hook_with_timeout(handler_fut).await {
                Some(Ok(())) => {}
                Some(Err(payload)) => tracing::warn!(
                    hook = "output_observe",
                    recovered = true,
                    cause = %panic_payload_to_string(&*payload),
                    "钩子执行 panic 已恢复"
                ),
                None => tracing::warn!(
                    hook = "output_observe",
                    timeout_secs = self.hook_timeout.as_secs(),
                    "钩子执行超时已跳过"
                ),
            }
        }
    }

    /// 初始化发送输入事件钩子：每个 session 装配时调用一次，传入该 session 的 sender
    ///
    /// 每个钩子收到 [`SessionSender`] 后自行保存，后续可随时调用其方法发送消息。
    /// 这是真·主动模式：插件自主决定何时发送，引擎只负责消费。
    ///
    /// SessionSender 绑定的是该 session 的三条通道（不是全局 tx），
    /// 多 session 并发时各 session 的 sender 完全隔离。
    pub async fn init_send_inputs(&mut self, sender: SessionSender) {
        self.ensure_sorted();
        for entry in &self.send_input {
            let handler_fut = AssertUnwindSafe((entry.handler)(sender.clone())).catch_unwind();
            match self.run_hook_with_timeout(handler_fut).await {
                Some(Ok(())) => {}
                Some(Err(payload)) => tracing::warn!(
                    hook = "send_input",
                    recovered = true,
                    cause = %panic_payload_to_string(&*payload),
                    "钩子执行 panic 已恢复"
                ),
                None => tracing::warn!(
                    hook = "send_input",
                    timeout_secs = self.hook_timeout.as_secs(),
                    "钩子执行超时已跳过"
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::SessionSender;
    use fuyao_api::message::input::{InterruptMessage, PluginEventSource, PluginMessage};
    use fuyao_api::message::output::UserMessage as OutputUserMessage;
    use std::sync::Arc;

    /// 构造测试用 SessionSender + 三条接收端（identity="test_plugin"）
    fn make_sender() -> (
        SessionSender,
        tokio::sync::mpsc::Receiver<OutputUserMessage>,
        tokio::sync::mpsc::Receiver<InterruptMessage>,
        tokio::sync::mpsc::Receiver<PluginMessage>,
    ) {
        let (tx_user, rx_user) = tokio::sync::mpsc::channel(16);
        let (tx_interrupt, rx_interrupt) = tokio::sync::mpsc::channel(16);
        let (tx_plugin, rx_plugin) = tokio::sync::mpsc::channel(16);
        let sender = SessionSender::new(
            PluginEventSource {
                name: "test_plugin".into(),
            },
            tx_user,
            tx_interrupt,
            tx_plugin,
        );
        (sender, rx_user, rx_interrupt, rx_plugin)
    }

    /// init_send_inputs 调用所有 send_input hook，传入 SessionSender
    #[tokio::test]
    async fn init_send_inputs_calls_handler_with_sender() {
        let (sender, mut rx_user, _rx_interrupt, _rx_plugin) = make_sender();
        let mut reg = HooksRegistry::new();
        let called = Arc::new(std::sync::Mutex::new(false));
        let called_clone = called.clone();
        reg.register_send_input(
            0,
            Arc::new(move |sender| {
                let called = called_clone.clone();
                Box::pin(async move {
                    *called.lock().unwrap() = true;
                    sender.send_user("引导消息");
                })
            }),
        );
        reg.init_send_inputs(sender).await;
        assert!(*called.lock().unwrap(), "hook 应被调用");
        let received = rx_user.recv().await.expect("应收到 User 消息");
        assert_eq!(received.payload.content, "引导消息");
    }

    /// 空 send_input 列表时 init_send_inputs 不 panic
    #[tokio::test]
    async fn init_send_input_noop_when_empty() {
        let (sender, _rx_user, _rx_interrupt, _rx_plugin) = make_sender();
        let mut reg = HooksRegistry::new();
        reg.init_send_inputs(sender).await;
    }

    /// 单个 hook panic 不阻塞后续 hook
    #[tokio::test]
    async fn init_send_input_panic_protection() {
        let (sender, mut rx_user, _rx_interrupt, _rx_plugin) = make_sender();
        let mut reg = HooksRegistry::new();
        reg.register_send_input(
            0,
            Arc::new(|_sender| Box::pin(async { panic!("钩子崩溃") })),
        );
        reg.register_send_input(
            0,
            Arc::new(|sender| {
                Box::pin(async move {
                    sender.send_user("降级消息");
                })
            }),
        );
        reg.init_send_inputs(sender).await;
        let received = rx_user.recv().await.expect("panic 后正常 hook 仍应执行");
        assert_eq!(received.payload.content, "降级消息");
    }

    /// 观察钩子按注册顺序串行执行
    #[tokio::test]
    async fn hook_output_observe_executes_in_registration_order() {
        use fuyao_api::message::EventBase;
        use fuyao_api::message::output::{ChunkMessage, ChunkPayload};

        let log = Arc::new(std::sync::Mutex::new(Vec::new()));

        let mut reg = HooksRegistry::new();
        for i in 0..3 {
            let log = log.clone();
            reg.register_output_observe(Arc::new(move |_msg| {
                let log = log.clone();
                Box::pin(async move {
                    log.lock().unwrap().push(i);
                })
            }));
        }

        let msg = OutputEvent::Chunk(ChunkMessage {
            base: EventBase::default(),
            payload: ChunkPayload {
                content: None,
                reasoning: None,
            },
        });
        reg.hook_output_observe(msg).await;

        let log = log.lock().unwrap();
        assert_eq!(*log, vec![0, 1, 2], "观察钩子应按注册顺序串行执行");
    }

    /// 观察钩子 panic 防护：单个钩子崩溃不阻塞后续钩子
    #[tokio::test]
    async fn hook_output_observe_panic_protection() {
        use fuyao_api::message::EventBase;
        use fuyao_api::message::output::{ChunkMessage, ChunkPayload};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let executed = Arc::new(AtomicUsize::new(0));

        let mut reg = HooksRegistry::new();

        // hook A: 正常
        let counter_a = executed.clone();
        reg.register_output_observe(Arc::new(move |_msg| {
            let c = counter_a.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        }));

        // hook B: panic
        reg.register_output_observe(Arc::new(|_msg| {
            Box::pin(async {
                panic!("观察钩子崩溃");
            })
        }));

        // hook C: 正常
        let counter_c = executed.clone();
        reg.register_output_observe(Arc::new(move |_msg| {
            let c = counter_c.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        }));

        let msg = OutputEvent::Chunk(ChunkMessage {
            base: EventBase::default(),
            payload: ChunkPayload {
                content: None,
                reasoning: None,
            },
        });
        reg.hook_output_observe(msg).await;

        // panic 防护：A 和 C 都执行了（B panic 不阻塞）
        assert_eq!(executed.load(Ordering::SeqCst), 2);
    }

    /// 超时防护：慢 hook 在超时后被跳过，不阻塞后续 hook
    #[tokio::test]
    async fn hook_output_observe_timeout_skips_slow_hook() {
        use fuyao_api::message::EventBase;
        use fuyao_api::message::output::{ChunkMessage, ChunkPayload};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut reg = HooksRegistry::new();
        // 设很短的超时（50ms）
        reg.hook_timeout = Duration::from_millis(50);

        // 慢 hook：sleep 10s
        reg.register_output_observe(Arc::new(|_msg| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(10)).await;
            })
        }));

        // 正常 hook
        let called = Arc::new(AtomicUsize::new(0));
        let called_clone = called.clone();
        reg.register_output_observe(Arc::new(move |_msg| {
            let c = called_clone.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        }));

        let msg = OutputEvent::Chunk(ChunkMessage {
            base: EventBase::default(),
            payload: ChunkPayload {
                content: None,
                reasoning: None,
            },
        });

        // 应在 ~50ms 内返回（不是 10s）
        let start = std::time::Instant::now();
        reg.hook_output_observe(msg).await;
        let elapsed = start.elapsed();

        // 慢 hook 超时被跳过，正常 hook 仍执行
        assert_eq!(called.load(Ordering::SeqCst), 1);
        // 确认没有等 10s（给足余量到 5s）
        assert!(
            elapsed < Duration::from_secs(5),
            "超时未生效，耗时 {elapsed:?}"
        );
    }
}
