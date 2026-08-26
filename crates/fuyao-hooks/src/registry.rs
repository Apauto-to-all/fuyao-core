//! Hooks 注册表与执行引擎
//!
//! 拦截钩子（同步串行原地修改，可阻止带原因，panic 防护）+
//! 观察钩子（异步串行，Arc 共享只读，panic 防护 + 超时防护）。
//! 两类钩子统一按优先级排序执行（priority 降序、同优先级按注册序，
//! [`HooksRegistry::finalize`] 装配期一次排定）。
//!
//! 故障防护覆盖（三类钩子故障形态各归其位，均为保留项）：
//! - 钩子 panic：`catch_unwind` 兜底，两类钩子都有
//! - observe 钩子挂起（future 永不完成）：超时兜底——本注册表内**唯一**保底机制
//! - intercept 钩子死循环（同步代码）：同步代码不可抢占，无运行时保底可做，
//!   属同步拦截的固有约束，靠插件作者自律
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
use std::sync::Arc;
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

    /// 注册输出观察钩子
    ///
    /// `priority` 高者先执行；同优先级按注册顺序（finalize 用稳定排序）。
    /// 与拦截钩子对称的优先级语义。
    pub fn register_output_observe(&mut self, priority: i32, handler: OutputObserveFn) {
        self.output_observe.push(Prioritized { priority, handler });
    }

    /// 冻结注册表：按 priority 降序排定拦截与观察两类钩子（高优先级先执行）
    ///
    /// 装配方在所有 register 完成后调用一次；之后注册表包进 `Arc`
    /// 进入运行期只读状态。稳定排序保证同优先级保持注册顺序。
    pub fn finalize(&mut self) {
        self.output_intercept
            .sort_by_key(|b| std::cmp::Reverse(b.priority));
        self.output_observe
            .sort_by_key(|b| std::cmp::Reverse(b.priority));
    }

    /// 带超时执行 hook future。
    ///
    /// - `hook_timeout` 为零：不超时，直接 await
    /// - `hook_timeout` > 0：用 `tokio::time::timeout` 包裹，超时返回 None
    ///
    /// 调用方负责先做 `catch_unwind`（panic 防护），本方法只管超时。
    ///
    /// 超时是 observe 钩子「挂起」故障的唯一保底，不可删减：
    /// - observe 串行 await 在 dispatch 内联路径上（ReAct 主循环与历史入口均
    ///   内联等待 deliver），一个永不完成的 future 会冻结整个 session 的
    ///   事件管道，且无日志、无诊断——无界静默假死
    /// - 插件面向二次开发者，钩子代码不受引擎作者控制，引擎必须兜底
    /// - 超时把无界冻结降为有界损失：丢弃该 future（副作用停在中间态）并告警，
    ///   管道继续推进；中间态风险小于整个 session 假死
    async fn run_hook_with_timeout<T>(&self, fut: impl Future<Output = T>) -> Option<T> {
        if self.hook_timeout.is_zero() {
            Some(fut.await)
        } else {
            tokio::time::timeout(self.hook_timeout, fut).await.ok()
        }
    }

    /// 执行输出拦截钩子：串行原地修改，panic 防护，任一阻止则立即短路
    ///
    /// 每个钩子拿到 `&mut` 事件原地修改——借用检查天然保证链式串行，
    /// 前一个钩子的修改对后续钩子可见。返回 `Some(reason)` 表示某钩子
    /// 阻止了该事件（携带原因），调用方据此丢弃事件；`None` 表示全部通过。
    pub fn hook_output_intercept(&self, msg: &mut OutputEvent) -> Option<String> {
        for entry in &self.output_intercept {
            // &mut 借用穿过 catch_unwind 边界需 AssertUnwindSafe：钩子 panic 后
            // 事件仍是合法内存值，继续用当前值走下一个钩子
            match std::panic::catch_unwind(AssertUnwindSafe(|| (entry.handler)(msg))) {
                Ok(None) => {}
                Ok(Some(reason)) => return Some(reason),
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
        None
    }

    /// 执行输出观察钩子：串行，panic 防护，超时防护
    ///
    /// 按 priority 降序（同优先级注册序）逐个执行，单个钩子 panic 或超时
    /// 不阻塞后续钩子。每个钩子拿到事件的 Arc 引用计数拷贝（事件本体共享只读）。
    pub async fn hook_output_observe(&self, msg: Arc<OutputEvent>) {
        for entry in &self.output_observe {
            // Arc::clone 只做引用计数拷贝（廉价），事件本体不复制
            let handler_fut = AssertUnwindSafe((entry.handler)(Arc::clone(&msg))).catch_unwind();
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

    /// 同优先级观察钩子按注册顺序串行执行
    #[tokio::test]
    async fn hook_output_observe_executes_in_registration_order() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let mut reg = HooksRegistry::new();
        for i in 0..3 {
            let log = log.clone();
            reg.register_output_observe(
                0,
                std::sync::Arc::new(move |_msg| {
                    let log = log.clone();
                    Box::pin(async move {
                        log.lock().unwrap().push(i);
                    })
                }),
            );
        }
        reg.finalize();

        reg.hook_output_observe(Arc::new(make_chunk())).await;

        let log = log.lock().unwrap();
        assert_eq!(*log, vec![0, 1, 2], "同优先级观察钩子应按注册顺序串行执行");
    }

    /// 观察钩子按 priority 降序执行（finalize 排定），同优先级保持注册顺序
    #[tokio::test]
    async fn observe_executes_in_priority_order_after_finalize() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let mut reg = HooksRegistry::new();
        // 低优先级先注册、高优先级后注册：finalize 后高者仍先执行
        for (priority, tag) in [(0, "low"), (10, "high")] {
            let log = log.clone();
            reg.register_output_observe(
                priority,
                std::sync::Arc::new(move |_msg| {
                    let log = log.clone();
                    Box::pin(async move {
                        log.lock().unwrap().push(tag);
                    })
                }),
            );
        }
        reg.finalize();

        reg.hook_output_observe(Arc::new(make_chunk())).await;

        let log = log.lock().unwrap();
        assert_eq!(*log, vec!["high", "low"], "高优先级观察钩子应先执行");
    }

    /// 观察钩子 panic 防护：单个钩子崩溃不阻塞后续钩子
    #[tokio::test]
    async fn hook_output_observe_panic_protection() {
        let executed = std::sync::Arc::new(AtomicUsize::new(0));

        let mut reg = HooksRegistry::new();

        // hook A: 正常
        let counter_a = executed.clone();
        reg.register_output_observe(
            0,
            std::sync::Arc::new(move |_msg| {
                let c = counter_a.clone();
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                })
            }),
        );

        // hook B: panic
        reg.register_output_observe(
            0,
            std::sync::Arc::new(|_msg| {
                Box::pin(async {
                    panic!("观察钩子崩溃");
                })
            }),
        );

        // hook C: 正常
        let counter_c = executed.clone();
        reg.register_output_observe(
            0,
            std::sync::Arc::new(move |_msg| {
                let c = counter_c.clone();
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                })
            }),
        );
        reg.finalize();

        reg.hook_output_observe(Arc::new(make_chunk())).await;

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
        reg.register_output_observe(
            0,
            std::sync::Arc::new(|_msg| {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                })
            }),
        );

        // 正常 hook
        let called = std::sync::Arc::new(AtomicUsize::new(0));
        let called_clone = called.clone();
        reg.register_output_observe(
            0,
            std::sync::Arc::new(move |_msg| {
                let c = called_clone.clone();
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                })
            }),
        );

        // 应在 ~50ms 内返回（不是 10s）
        let start = std::time::Instant::now();
        reg.hook_output_observe(Arc::new(make_chunk())).await;
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
                    None
                }),
            );
        }
        reg.finalize();

        let mut ev = make_chunk();
        let result = reg.hook_output_intercept(&mut ev);

        assert!(result.is_none(), "未阻止时应返回 None");
        let log = log.lock().unwrap();
        assert_eq!(*log, vec!["high", "low"], "高优先级拦截钩子应先执行");
    }

    /// 拦截钩子原地修改链式生效：前一个钩子的修改对后续钩子可见
    #[test]
    fn intercept_mutations_chain_in_place() {
        let mut reg = HooksRegistry::new();
        // 高优先级钩子先写入一层内容
        reg.register_output_intercept(
            10,
            std::sync::Arc::new(|ev| {
                if let OutputEvent::Chunk(msg) = ev {
                    msg.payload.content = Some("第一层".into());
                }
                None
            }),
        );
        // 低优先级钩子应看到高优先级钩子已写入的内容，并在其上追加
        reg.register_output_intercept(
            0,
            std::sync::Arc::new(|ev| {
                if let OutputEvent::Chunk(msg) = ev {
                    let prev = msg.payload.content.clone().unwrap_or_default();
                    msg.payload.content = Some(format!("{prev}+第二层"));
                }
                None
            }),
        );
        reg.finalize();

        let mut ev = make_chunk();
        assert!(reg.hook_output_intercept(&mut ev).is_none());
        let content = match ev {
            OutputEvent::Chunk(msg) => msg.payload.content,
            _ => None,
        };
        assert_eq!(content.as_deref(), Some("第一层+第二层"));
    }

    /// 拦截钩子阻止语义：返回 Some(reason) 立即短路，后续钩子不再执行
    #[test]
    fn intercept_block_short_circuits_remaining_hooks() {
        let executed = std::sync::Arc::new(AtomicUsize::new(0));

        let mut reg = HooksRegistry::new();
        // 高优先级钩子阻止事件
        reg.register_output_intercept(
            10,
            std::sync::Arc::new(|_ev| Some("被高优先级钩子阻止".into())),
        );
        // 低优先级钩子不应被执行
        let executed_low = executed.clone();
        reg.register_output_intercept(
            0,
            std::sync::Arc::new(move |_ev| {
                executed_low.fetch_add(1, Ordering::SeqCst);
                None
            }),
        );
        reg.finalize();

        let mut ev = make_chunk();
        let result = reg.hook_output_intercept(&mut ev);

        assert_eq!(result, Some("被高优先级钩子阻止".to_string()));
        assert_eq!(
            executed.load(Ordering::SeqCst),
            0,
            "阻止后低优先级钩子不应执行"
        );
    }

    /// 拦截钩子 panic 防护：单个钩子崩溃不阻塞后续钩子，事件继续传递
    #[test]
    fn intercept_panic_protection() {
        let mut reg = HooksRegistry::new();
        // 高优先级钩子 panic
        reg.register_output_intercept(
            10,
            std::sync::Arc::new(|_ev| {
                panic!("拦截钩子崩溃");
            }),
        );
        // 低优先级钩子仍应收到事件并正常修改
        reg.register_output_intercept(
            0,
            std::sync::Arc::new(|ev| {
                if let OutputEvent::Chunk(msg) = ev {
                    msg.payload.content = Some("panic 后仍被低优先级钩子修改".into());
                }
                None
            }),
        );
        reg.finalize();

        let mut ev = make_chunk();
        assert!(reg.hook_output_intercept(&mut ev).is_none());
        let content = match ev {
            OutputEvent::Chunk(msg) => msg.payload.content,
            _ => None,
        };
        assert_eq!(content.as_deref(), Some("panic 后仍被低优先级钩子修改"));
    }
}
