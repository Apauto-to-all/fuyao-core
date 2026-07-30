//! fuyao-hooks 集成测试：钩子执行语义（优先级、短路、panic 防护）
//!
//! registry.rs 单元测试覆盖了 observe/send_input 的 panic 防护与超时。
//! 本文件聚焦 intercept 执行语义的公共 API 契约——单测的空白带：
//!
//! - **intercept 优先级排序**：多 intercept 按 priority 降序执行（需先触发 ensure_sorted）
//! - **惰性排序契约**（隐藏契约，需钉死）：ensure_sorted 仅由 init_send_inputs 触发，
//!   hook_output_intercept 不触发排序。不经 init_send_inputs 直接 intercept 时，
//!   执行顺序 = 注册顺序而非 priority 顺序
//! - **Block 短路**：任一 intercept 返回 Block 立即终止，后续 intercept 不执行
//! - **intercept panic 防护**：单测完全未覆盖 intercept 的 catch_unwind 路径（registry.rs
//!   只测了 observe/send_input 的 panic），此处补足
//! - **intercept 串联修改**：多个 Pass 的 intercept 链式 transform 同一事件
//!
//! 全部使用默认配置（不调 set_config），走 get_config 未 set 返回 default 的兜底。

mod common;

use std::sync::{Arc, Mutex};

use common::make_chunk;
use fuyao_api::message::OutputEvent;
use fuyao_hooks::{HooksRegistry, InterceptResult, OutputInterceptFn};

/// 构造空 registry（独立测试二进制内 OnceLock 未 set，走 default 兜底）
fn new_registry() -> HooksRegistry {
    HooksRegistry::new()
}

/// 直接向 registry 注册 intercept 钩子，记录其执行（log_tag）
fn record_intercept(reg: &mut HooksRegistry, priority: i32, log: &common::ExecLog, tag: &str) {
    let log = log.clone();
    let tag = tag.to_string();
    let handler: OutputInterceptFn = Arc::new(move |msg| {
        log.lock().unwrap().push(tag.clone());
        InterceptResult::Pass(msg.clone())
    });
    reg.register_output_intercept(priority, handler);
}

/// 向 registry 注册返回 Block 的 intercept 钩子（reason, 记录 log_tag）
fn block_intercept(
    reg: &mut HooksRegistry,
    priority: i32,
    log: &common::ExecLog,
    tag: &str,
    reason: &str,
) {
    let log = log.clone();
    let tag = tag.to_string();
    let reason = reason.to_string();
    let handler: OutputInterceptFn = Arc::new(move |_msg| {
        log.lock().unwrap().push(tag.clone());
        InterceptResult::Block(reason.clone())
    });
    reg.register_output_intercept(priority, handler);
}

/// 注册一个会 panic 的 intercept 钩子
fn panic_intercept(reg: &mut HooksRegistry, priority: i32, log: &common::ExecLog, tag: &str) {
    let log = log.clone();
    let tag = tag.to_string();
    let handler: OutputInterceptFn = Arc::new(move |_msg| {
        log.lock().unwrap().push(tag.clone());
        panic!("intercept 钩子崩溃");
    });
    reg.register_output_intercept(priority, handler);
}

// ============================================================================
// 优先级排序（经 init_send_inputs 触发 ensure_sorted 后）
// ============================================================================

#[tokio::test]
async fn intercept_runs_in_priority_descending_after_sort() {
    // 经 init_send_inputs 触发 ensure_sorted 后，intercept 按 priority 降序执行。
    // 注册顺序故意与优先级相反（低优先级先注册），验证排序生效。
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let mut reg = new_registry();
    record_intercept(&mut reg, 1, &log, "low"); // priority=1
    record_intercept(&mut reg, 10, &log, "high"); // priority=10
    record_intercept(&mut reg, 5, &log, "mid"); // priority=5

    // 触发排序：init_send_inputs 内部调 ensure_sorted
    let (_sender, _rx_user, _rx_interrupt, _rx_plugin) = common::make_sender();
    reg.init_send_inputs(_sender).await;

    reg.hook_output_intercept(&OutputEvent::Chunk(make_chunk("e")));

    let recorded = log.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec!["high", "mid", "low"],
        "排序后应按 priority 降序：high(10) → mid(5) → low(1)"
    );
}

// ============================================================================
// 惰性排序契约（隐藏契约，钉死）
// ============================================================================

#[tokio::test]
async fn intercept_without_init_runs_in_registration_order() {
    // 隐藏契约：hook_output_intercept 不调 ensure_sorted。
    // 不经 init_send_inputs 直接 intercept 时，执行顺序 = 注册顺序（非 priority 顺序）。
    // 钉死此契约：调用方必须先 init_send_inputs 才能享受优先级排序。
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let mut reg = new_registry();
    record_intercept(&mut reg, 1, &log, "low"); // 注册第 1
    record_intercept(&mut reg, 10, &log, "high"); // 注册第 2

    // 关键：不调 init_send_inputs，直接 intercept
    reg.hook_output_intercept(&OutputEvent::Chunk(make_chunk("e")));

    let recorded = log.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec!["low", "high"],
        "未经排序时执行顺序 = 注册顺序（非 priority 顺序）"
    );
}

#[tokio::test]
async fn init_send_inputs_sorts_once_subsequent_intercept_respects_priority() {
    // ensure_sorted 用 dirty 标记，只排一次。init_send_inputs 排序后，
    // 后续注册的新 intercept 会重置 dirty，但未再次 init 时仍按「上次排序结果 + 追加」执行。
    // 验证排序的惰性：一次排序后 registry 保持稳定。
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let mut reg = new_registry();
    record_intercept(&mut reg, 1, &log, "low");
    record_intercept(&mut reg, 10, &log, "high");

    let (sender, _rx_user, _rx_interrupt, _rx_plugin) = common::make_sender();
    reg.init_send_inputs(sender).await; // 排序：high, low

    // 第一次 intercept 走排序后顺序
    reg.hook_output_intercept(&OutputEvent::Chunk(make_chunk("e1")));
    let first = log.lock().unwrap().clone();
    assert_eq!(first, vec!["high", "low"]);

    // 第二次 intercept 应保持同样的排序顺序（dirty 已清，不重排）
    log.lock().unwrap().clear();
    reg.hook_output_intercept(&OutputEvent::Chunk(make_chunk("e2")));
    let second = log.lock().unwrap().clone();
    assert_eq!(
        second,
        vec!["high", "low"],
        "排序结果应稳定，不因重复 intercept 变化"
    );
}

// ============================================================================
// Block 短路
// ============================================================================

#[tokio::test]
async fn intercept_block_short_circuits_remaining_handlers() {
    // 任一 intercept 返回 Block 立即终止，后续 intercept 不执行。
    // 即使高优先级的 intercept 放行，中间一个 Block 也应短路。
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let mut reg = new_registry();
    record_intercept(&mut reg, 10, &log, "high_pass"); // 放行
    block_intercept(&mut reg, 5, &log, "mid_block", "被阻止"); // priority=5 Block
    record_intercept(&mut reg, 1, &log, "low_never"); // 不应执行

    let (sender, _rx_user, _rx_interrupt, _rx_plugin) = common::make_sender();
    reg.init_send_inputs(sender).await; // 排序：high_pass, mid_block, low_never

    let result = reg.hook_output_intercept(&OutputEvent::Chunk(make_chunk("e")));

    match result {
        InterceptResult::Block(reason) => assert_eq!(reason, "被阻止"),
        _ => panic!("应返回 Block"),
    }
    let recorded = log.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec!["high_pass", "mid_block"],
        "Block 后 low_never 不应执行"
    );
}

// ============================================================================
// intercept 串联修改
// ============================================================================

#[tokio::test]
async fn intercept_chain_pass_modifies_event_in_sequence() {
    // 多个 Pass 的 intercept 链式 transform 同一事件：每个拿到前一个的修改结果。
    // 用 content 字段串联追加，验证 current = modified 的传递。
    let mut reg = new_registry();
    // 第一个 intercept：content 追加 "-A"
    reg.register_output_intercept(
        1,
        Arc::new(|msg| {
            if let OutputEvent::Chunk(mut c) = msg.clone() {
                if let Some(ref mut s) = c.payload.content {
                    s.push_str("-A");
                }
                InterceptResult::Pass(OutputEvent::Chunk(c))
            } else {
                InterceptResult::Pass(msg.clone())
            }
        }),
    );
    // 第二个 intercept：content 追加 "-B"（应看到 "-A" 已生效）
    let seen = Arc::new(Mutex::new(String::new()));
    let seen_clone = seen.clone();
    reg.register_output_intercept(
        2,
        Arc::new(move |msg| {
            if let OutputEvent::Chunk(c) = msg {
                *seen_clone.lock().unwrap() = c.payload.content.clone().unwrap_or_default();
            }
            InterceptResult::Pass(msg.clone())
        }),
    );

    let (sender, _rx_user, _rx_interrupt, _rx_plugin) = common::make_sender();
    reg.init_send_inputs(sender).await; // 排序：priority 2 先，1 后

    let result = reg.hook_output_intercept(&OutputEvent::Chunk(make_chunk("base")));

    // priority 2 先执行，此时 content 还没被 priority 1 追加
    let seen_by_second = seen.lock().unwrap().clone();
    assert_eq!(seen_by_second, "base", "高优先级先执行，此时应看到原始内容");

    // 最终结果：priority 2 放行原样 → priority 1 追加 "-A"
    if let InterceptResult::Pass(OutputEvent::Chunk(c)) = result {
        assert_eq!(c.payload.content.as_deref(), Some("base-A"));
    } else {
        panic!("应放行修改后的事件");
    }
}

// ============================================================================
// intercept panic 防护（单测空白，补足）
// ============================================================================

#[tokio::test]
async fn intercept_panic_does_not_block_subsequent_handlers() {
    // intercept 的 catch_unwind 路径在 registry.rs 单测中完全未覆盖。
    // 单个 intercept panic 应被恢复，后续 intercept 仍执行。
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let mut reg = new_registry();
    panic_intercept(&mut reg, 10, &log, "boom"); // 高优先级 panic
    record_intercept(&mut reg, 1, &log, "survivor"); // 应继续执行

    let (sender, _rx_user, _rx_interrupt, _rx_plugin) = common::make_sender();
    reg.init_send_inputs(sender).await; // 排序：boom, survivor

    // panic 被恢复，不应传播；survivor 仍执行
    let result = reg.hook_output_intercept(&OutputEvent::Chunk(make_chunk("e")));
    assert!(
        matches!(result, InterceptResult::Pass(_)),
        "panic 恢复后应继续到 Pass"
    );

    let recorded = log.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec!["boom", "survivor"],
        "boom panic 被恢复后 survivor 仍应执行"
    );
}

#[tokio::test]
async fn intercept_all_panic_returns_original_event() {
    // 所有 intercept 都 panic：全部被恢复，返回原始事件（current 从未被修改）
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let mut reg = new_registry();
    panic_intercept(&mut reg, 5, &log, "boom1");
    panic_intercept(&mut reg, 3, &log, "boom2");

    let (sender, _rx_user, _rx_interrupt, _rx_plugin) = common::make_sender();
    reg.init_send_inputs(sender).await;

    let original = OutputEvent::Chunk(make_chunk("untouched"));
    let result = reg.hook_output_intercept(&original);
    if let InterceptResult::Pass(OutputEvent::Chunk(c)) = result {
        assert_eq!(c.payload.content.as_deref(), Some("untouched"));
    } else {
        panic!("全 panic 时应放行原始事件");
    }
}

// ============================================================================
// observe 与 intercept 互不干扰（语义正交）
// ============================================================================

#[tokio::test]
async fn observe_does_not_affect_intercept_result() {
    // observe 是只读副作用，intercept 是可修改/阻止。两者在同一 registry 共存时，
    // observe 的执行不影响 intercept 的返回值（语义正交）。
    let observe_count = Arc::new(Mutex::new(0u32));
    let mut reg = new_registry();

    let count = observe_count.clone();
    reg.register_output_observe(Arc::new(move |_msg| {
        let count = count.clone();
        Box::pin(async move {
            *count.lock().unwrap() += 1;
        })
    }));
    record_intercept(&mut reg, 0, &Arc::new(Mutex::new(vec![])), "noop");

    let event = OutputEvent::Chunk(make_chunk("payload"));
    reg.hook_output_observe(event.clone()).await;
    let result = reg.hook_output_intercept(&event);

    assert_eq!(*observe_count.lock().unwrap(), 1, "observe 应执行一次");
    assert!(matches!(result, InterceptResult::Pass(_)));
    // content 不被 observe 修改
    if let InterceptResult::Pass(OutputEvent::Chunk(c)) = result {
        assert_eq!(c.payload.content.as_deref(), Some("payload"));
    }
}
