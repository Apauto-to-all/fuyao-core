//! Hooks 注册表与执行引擎
//!
//! 拦截钩子（异步串行，可取消带原因，panic 防护）+ 观察钩子（异步串行）。
//! 按优先级排序执行。

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
    /// before_llm 钩子列表
    before_llm: Vec<Prioritized<BeforeLlmFn>>,
    /// 输出拦截钩子列表
    output_intercept: Vec<Prioritized<OutputInterceptFn>>,
    /// 输出观察钩子列表
    output_observe: Vec<Prioritized<OutputObserveFn>>,
    /// LLM 错误决策钩子列表
    on_llm_error: Vec<Prioritized<OnLlmErrorFn>>,
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
            before_llm: Vec::new(),
            output_intercept: Vec::new(),
            output_observe: Vec::new(),
            on_llm_error: Vec::new(),
            send_input: Vec::new(),
            dirty: false,
            hook_timeout: Duration::from_secs(timeout_secs),
        }
    }

    pub fn register_before_llm(&mut self, priority: i32, handler: BeforeLlmFn) {
        self.before_llm.push(Prioritized { priority, handler });
        self.dirty = true;
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

    /// 注册 LLM 错误决策钩子
    pub fn register_on_llm_error(&mut self, priority: i32, handler: OnLlmErrorFn) {
        self.on_llm_error.push(Prioritized { priority, handler });
        self.dirty = true;
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
        sort_by_priority(&mut self.before_llm);
        sort_by_priority(&mut self.output_intercept);
        sort_by_priority(&mut self.on_llm_error);
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

    /// 执行 before_llm 钩子：异步串行，返回最后一个非空消息 + skip_tools OR 语义
    pub async fn hook_before_llm(&mut self) -> BeforeLlmOutput {
        self.ensure_sorted();
        let mut result = BeforeLlmOutput::default();
        for entry in &self.before_llm {
            let handler_fut = AssertUnwindSafe((entry.handler)()).catch_unwind();
            match self.run_hook_with_timeout(handler_fut).await {
                Some(Ok(output)) => {
                    if !output.messages.is_empty() {
                        result.messages = output.messages;
                    }
                    // 任一 hook 请求 skip_tools 则生效（OR 语义）
                    if output.skip_tools {
                        result.skip_tools = true;
                    }
                }
                Some(Err(payload)) => {
                    tracing::warn!(
                        hook = "before_llm",
                        recovered = true,
                        cause = %panic_payload_to_string(&*payload),
                        "钩子执行 panic 已恢复"
                    );
                }
                None => {
                    tracing::warn!(
                        hook = "before_llm",
                        timeout_secs = self.hook_timeout.as_secs(),
                        "钩子执行超时已跳过"
                    );
                }
            }
        }
        result
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

    /// 执行 LLM 错误决策钩子：串行，panic 防护，首个非 Retry 结果即返回
    ///
    /// 默认行为：无钩子或所有钩子返回 Retry 时，返回 Retry（无限重试）
    pub fn hook_on_llm_error(&self, error: &str, retry_count: u32) -> LlmErrorAction {
        for entry in &self.on_llm_error {
            match std::panic::catch_unwind(AssertUnwindSafe(|| (entry.handler)(error, retry_count)))
            {
                Ok(LlmErrorAction::Retry) => continue,
                Ok(action) => return action,
                Err(payload) => {
                    tracing::warn!(
                        hook = "on_llm_error",
                        recovered = true,
                        cause = %panic_payload_to_string(&*payload),
                        "钩子执行 panic 已恢复"
                    );
                    continue;
                }
            }
        }
        LlmErrorAction::Retry
    }

    /// 初始化发送输入事件钩子：引擎启动时调用一次，传入 Sender
    ///
    /// 每个钩子收到 Sender 后自行保存，后续可随时 try_send。
    /// 这是真·主动模式，不依赖 emit 调用频率。
    pub async fn init_send_inputs(
        &mut self,
        tx: tokio::sync::mpsc::Sender<fuyao_api::message::InputEvent>,
    ) {
        self.ensure_sorted();
        for entry in &self.send_input {
            let handler_fut = AssertUnwindSafe((entry.handler)(tx.clone())).catch_unwind();
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
    use fuyao_api::Message;
    use std::sync::Arc;

    #[test]
    fn hooks_registry_new_is_empty() {
        let reg = HooksRegistry::new();
        assert!(reg.before_llm.is_empty());
    }

    #[tokio::test]
    async fn hook_before_llm_returns_last_non_empty() {
        let mut reg = HooksRegistry::new();
        reg.register_before_llm(
            0,
            Arc::new(|| {
                Box::pin(async {
                    BeforeLlmOutput {
                        messages: vec![Message::user("first".to_string())],
                        skip_tools: false,
                    }
                })
            }),
        );
        reg.register_before_llm(
            0,
            Arc::new(|| {
                Box::pin(async {
                    BeforeLlmOutput {
                        messages: vec![Message::user("second".to_string())],
                        skip_tools: false,
                    }
                })
            }),
        );

        let result = reg.hook_before_llm().await;
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].content, Some("second".to_string()));
        assert!(!result.skip_tools);
    }

    #[tokio::test]
    async fn hook_before_llm_priority_order() {
        let mut reg = HooksRegistry::new();
        reg.register_before_llm(
            1,
            Arc::new(|| {
                Box::pin(async {
                    BeforeLlmOutput {
                        messages: vec![Message::user("high".to_string())],
                        skip_tools: false,
                    }
                })
            }),
        );
        reg.register_before_llm(
            -1,
            Arc::new(|| {
                Box::pin(async {
                    BeforeLlmOutput {
                        messages: vec![Message::user("low".to_string())],
                        skip_tools: false,
                    }
                })
            }),
        );

        let result = reg.hook_before_llm().await;
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].content, Some("low".to_string()));
    }

    #[tokio::test]
    async fn hook_before_llm_skip_tools_or_semantics() {
        let mut reg = HooksRegistry::new();
        // hook A: skip_tools = false
        reg.register_before_llm(
            1,
            Arc::new(|| {
                Box::pin(async {
                    BeforeLlmOutput {
                        messages: vec![Message::user("a".to_string())],
                        skip_tools: false,
                    }
                })
            }),
        );
        // hook B: skip_tools = true
        reg.register_before_llm(
            0,
            Arc::new(|| {
                Box::pin(async {
                    BeforeLlmOutput {
                        messages: vec![Message::user("b".to_string())],
                        skip_tools: true,
                    }
                })
            }),
        );

        let result = reg.hook_before_llm().await;
        // OR 语义：任一 hook 设 true 即生效
        assert!(result.skip_tools);
        // 消息仍是最后一个非空
        assert_eq!(result.messages[0].content, Some("b".to_string()));
    }

    #[test]
    fn hook_on_llm_error_default_returns_retry() {
        let reg = HooksRegistry::new();
        let action = reg.hook_on_llm_error("timeout", 1);
        assert!(matches!(action, LlmErrorAction::Retry));
    }

    #[test]
    fn hook_on_llm_error_abort_action() {
        let mut reg = HooksRegistry::new();
        reg.register_on_llm_error(0, Arc::new(|_, _| LlmErrorAction::Abort));
        let action = reg.hook_on_llm_error("error", 1);
        assert!(matches!(action, LlmErrorAction::Abort));
    }

    #[test]
    fn hook_on_llm_error_panic_protection() {
        let mut reg = HooksRegistry::new();
        reg.register_on_llm_error(0, Arc::new(|_, _| panic!("钩子崩溃")));
        reg.register_on_llm_error(0, Arc::new(|_, _| LlmErrorAction::Abort));
        let action = reg.hook_on_llm_error("error", 1);
        assert!(matches!(action, LlmErrorAction::Abort));
    }

    #[tokio::test]
    async fn init_send_inputs_calls_handler_with_sender() {
        use fuyao_api::message::input::{UserMessage, UserPayload};
        use fuyao_api::message::{EventBase, InputEvent, UserMessageMode, UserMessageSource};
        let (tx, mut rx) = tokio::sync::mpsc::channel::<InputEvent>(10);
        let mut reg = HooksRegistry::new();
        let called = Arc::new(std::sync::Mutex::new(false));
        let called_clone = called.clone();
        reg.register_send_input(
            0,
            Arc::new(move |sender| {
                let called = called_clone.clone();
                Box::pin(async move {
                    *called.lock().unwrap() = true;
                    sender
                        .try_send(InputEvent::User(UserMessage {
                            base: EventBase::default(),
                            payload: UserPayload {
                                content: "引导消息".to_string(),
                                mode: UserMessageMode::Guide,
                                source: UserMessageSource::Plugin(
                                    fuyao_api::message::PluginSource {
                                        name: "hook".to_string(),
                                    },
                                ),
                            },
                        }))
                        .ok();
                })
            }),
        );
        reg.init_send_inputs(tx).await;
        assert!(*called.lock().unwrap());
        let received = rx.try_recv().unwrap();
        assert!(matches!(received, InputEvent::User(_)));
    }

    #[tokio::test]
    async fn init_send_input_noop_when_empty() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<fuyao_api::message::InputEvent>(10);
        let mut reg = HooksRegistry::new();
        reg.init_send_inputs(tx).await;
    }

    #[tokio::test]
    async fn init_send_input_panic_protection() {
        use fuyao_api::message::input::{UserMessage, UserPayload};
        use fuyao_api::message::{EventBase, InputEvent, UserMessageMode, UserMessageSource};
        let (tx, mut rx) = tokio::sync::mpsc::channel::<InputEvent>(10);
        let mut reg = HooksRegistry::new();
        reg.register_send_input(
            0,
            Arc::new(|_sender| Box::pin(async { panic!("钩子崩溃") })),
        );
        reg.register_send_input(
            0,
            Arc::new(|sender| {
                Box::pin(async move {
                    sender
                        .try_send(InputEvent::User(UserMessage {
                            base: EventBase::default(),
                            payload: UserPayload {
                                content: "降级消息".to_string(),
                                mode: UserMessageMode::Guide,
                                source: UserMessageSource::Plugin(
                                    fuyao_api::message::PluginSource {
                                        name: "hook".to_string(),
                                    },
                                ),
                            },
                        }))
                        .ok();
                })
            }),
        );
        reg.init_send_inputs(tx).await;
        let received = rx.try_recv().unwrap();
        assert!(matches!(received, InputEvent::User(_)));
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
