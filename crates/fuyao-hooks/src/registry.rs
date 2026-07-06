//! Hooks 注册表与执行引擎
//!
//! 拦截钩子（异步串行，可取消带原因，panic 防护）+ 观察钩子（异步串行）。
//! 按优先级排序执行。

use crate::types::*;
use futures_util::future::FutureExt;
use fuyao_api::message::OutputEvent;
use std::panic::AssertUnwindSafe;

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
}

impl Default for HooksRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HooksRegistry {
    pub fn new() -> Self {
        Self {
            before_llm: Vec::new(),
            output_intercept: Vec::new(),
            output_observe: Vec::new(),
            on_llm_error: Vec::new(),
            send_input: Vec::new(),
            dirty: false,
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

    /// 执行 before_llm 钩子：异步串行，返回最后一个非空消息 + skip_tools OR 语义
    pub async fn hook_before_llm(&mut self) -> BeforeLlmOutput {
        self.ensure_sorted();
        let mut result = BeforeLlmOutput::default();
        for entry in &self.before_llm {
            match AssertUnwindSafe((entry.handler)()).catch_unwind().await {
                Ok(output) => {
                    if !output.messages.is_empty() {
                        result.messages = output.messages;
                    }
                    // 任一 hook 请求 skip_tools 则生效（OR 语义）
                    if output.skip_tools {
                        result.skip_tools = true;
                    }
                }
                Err(_) => {
                    // panic 防护：跳过崩溃的钩子继续执行
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
                Err(_) => {}
            }
        }
        InterceptResult::Pass(current)
    }

    /// 执行输出观察钩子：串行，panic 防护
    ///
    /// 按注册顺序逐个执行，单个钩子 panic 不阻塞后续钩子。
    pub async fn hook_output_observe(&self, msg: OutputEvent) {
        for entry in &self.output_observe {
            let _ = AssertUnwindSafe((entry.handler)(msg.clone()))
                .catch_unwind()
                .await;
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
                Err(_) => continue,
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
            let _ = AssertUnwindSafe((entry.handler)(tx.clone()))
                .catch_unwind()
                .await;
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
}
