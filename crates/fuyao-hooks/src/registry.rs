//! Hooks 注册表与执行引擎
//!
//! 拦截钩子（同步串行，可取消带原因，panic 防护）+ 观察钩子（异步串行）。
//! 按优先级排序执行（[`HooksRegistry::finalize`] 装配期一次排定）。
//!
//! 生命周期约定：注册只发生在 session 装配期（`register_*` 需 `&mut self`），
//! 装配方在注册完成后调一次 [`HooksRegistry::finalize`] 排序冻结，之后注册表
//! 以 `Arc` 只读共享（见 [`crate::SharedHooks`]），运行期无锁、不可再改。
//! hook 闭包如持共享状态，需自行内部同步（如插件 state 的 Mutex）。

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
            hook_timeout: Duration::from_secs(timeout_secs),
        }
    }

    /// 注册输出拦截钩子
    ///
    /// `priority` 高者先执行；同优先级按注册顺序（finalize 用稳定排序）。
    pub fn register_output_intercept(&mut self, priority: i32, handler: OutputInterceptFn) {
        self.output_intercept
            .push(Prioritized { priority, handler });
    }

    /// 注册输出观察钩子（观察无优先级语义，按注册顺序串行）
    pub fn register_output_observe(&mut self, handler: OutputObserveFn) {
        self.output_observe.push(Prioritized {
            priority: 0,
            handler,
        });
    }

    /// 冻结注册表：按 priority 降序排定拦截钩子（高优先级先执行）
    ///
    /// 装配方在所有 register 完成后调用一次；之后注册表包进 `Arc`
    /// 进入运行期只读状态。稳定排序保证同优先级保持注册顺序。
    pub fn finalize(&mut self) {
        self.output_intercept
            .sort_by_key(|b| std::cmp::Reverse(b.priority));
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::message::EventBase;
    use fuyao_api::message::output::{ChunkMessage, ChunkPayload};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 构造测试用 Chunk 事件
    fn make_chunk() -> OutputEvent {
        OutputEvent::Chunk(ChunkMessage {
            base: EventBase::default(),
            payload: ChunkPayload {
                content: None,
                reasoning: None,
            },
        })
    }

    /// 观察钩子按注册顺序串行执行
    #[tokio::test]
    async fn hook_output_observe_executes_in_registration_order() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let mut reg = HooksRegistry::new();
        for i in 0..3 {
            let log = log.clone();
            reg.register_output_observe(std::sync::Arc::new(move |_msg| {
                let log = log.clone();
                Box::pin(async move {
                    log.lock().unwrap().push(i);
                })
            }));
        }

        reg.hook_output_observe(make_chunk()).await;

        let log = log.lock().unwrap();
        assert_eq!(*log, vec![0, 1, 2], "观察钩子应按注册顺序串行执行");
    }

    /// 观察钩子 panic 防护：单个钩子崩溃不阻塞后续钩子
    #[tokio::test]
    async fn hook_output_observe_panic_protection() {
        let executed = std::sync::Arc::new(AtomicUsize::new(0));

        let mut reg = HooksRegistry::new();

        // hook A: 正常
        let counter_a = executed.clone();
        reg.register_output_observe(std::sync::Arc::new(move |_msg| {
            let c = counter_a.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        }));

        // hook B: panic
        reg.register_output_observe(std::sync::Arc::new(|_msg| {
            Box::pin(async {
                panic!("观察钩子崩溃");
            })
        }));

        // hook C: 正常
        let counter_c = executed.clone();
        reg.register_output_observe(std::sync::Arc::new(move |_msg| {
            let c = counter_c.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        }));

        reg.hook_output_observe(make_chunk()).await;

        // panic 防护：A 和 C 都执行了（B panic 不阻塞）
        assert_eq!(executed.load(Ordering::SeqCst), 2);
    }

    /// 超时防护：慢 hook 在超时后被跳过，不阻塞后续 hook
    #[tokio::test]
    async fn hook_output_observe_timeout_skips_slow_hook() {
        let mut reg = HooksRegistry::new();
        // 设很短的超时（50ms）
        reg.hook_timeout = Duration::from_millis(50);

        // 慢 hook：sleep 10s
        reg.register_output_observe(std::sync::Arc::new(|_msg| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(10)).await;
            })
        }));

        // 正常 hook
        let called = std::sync::Arc::new(AtomicUsize::new(0));
        let called_clone = called.clone();
        reg.register_output_observe(std::sync::Arc::new(move |_msg| {
            let c = called_clone.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        }));

        // 应在 ~50ms 内返回（不是 10s）
        let start = std::time::Instant::now();
        reg.hook_output_observe(make_chunk()).await;
        let elapsed = start.elapsed();

        // 慢 hook 超时被跳过，正常 hook 仍执行
        assert_eq!(called.load(Ordering::SeqCst), 1);
        // 确认没有等 10s（给足余量到 5s）
        assert!(
            elapsed < Duration::from_secs(5),
            "超时未生效，耗时 {elapsed:?}"
        );
    }

    /// 拦截钩子按 priority 降序执行（finalize 排定），同优先级保持注册顺序
    #[test]
    fn intercept_executes_in_priority_order_after_finalize() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let mut reg = HooksRegistry::new();
        // 低优先级先注册、高优先级后注册：finalize 后高者仍先执行
        for (priority, tag) in [(0, "low"), (10, "high")] {
            let log = log.clone();
            reg.register_output_intercept(
                priority,
                std::sync::Arc::new(move |_ev| {
                    log.lock().unwrap().push(tag);
                    InterceptResult::Pass(_ev.clone())
                }),
            );
        }
        reg.finalize();

        let _ = reg.hook_output_intercept(&make_chunk());

        let log = log.lock().unwrap();
        assert_eq!(*log, vec!["high", "low"], "高优先级拦截钩子应先执行");
    }
}
