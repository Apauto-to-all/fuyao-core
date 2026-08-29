//! fuyao-hooks 集成测试：钩子执行语义（优先级、短路、panic 防护）
//!
//! registry.rs 单元测试覆盖了 observe 的 panic 防护与超时，以及 intercept 的
//! 原地修改 / 短路 / panic 路径。本文件聚焦 intercept 执行语义的公共 API 契约——
//! 以独立测试二进制钉死跨用例的稳定行为：
//!
//! - **intercept 优先级排序**：多 intercept 经 [`HooksRegistry::finalize`] 排定后
//!   按 priority 降序执行
//! - **冻结契约**（隐藏契约，需钉死）：finalize 是唯一排序点，
//!   hook_output_intercept 不触发排序。不经 finalize 直接 intercept 时，
//!   执行顺序 = 注册顺序而非 priority 顺序——调用方必须先 finalize
//! - **阻止短路**：任一 intercept 返回 `Some(reason)` 立即终止，后续 intercept 不执行
//! - **intercept panic 防护**：单测覆盖了 catch_unwind 路径，此处从公共 API
//!   契约维度再钉一次（panic 恢复后事件继续走完链路）
//! - **intercept 原地串联修改**：多个放行的 intercept 链式原地修改同一事件
//!
//! 全部使用默认配置（不调 set_config），走 get_config 未 set 返回 default 的兜底。

mod common;

use std::sync::{Arc, Mutex};

use common::make_chunk;
use fuyao_api::message::OutputEvent;
use fuyao_hooks::{HooksRegistry, OutputInterceptFn};

/// 构造空 registry（独立测试二进制内 OnceLock 未 set，走 default 兜底）
fn new_registry() -> HooksRegistry {
    HooksRegistry::new()
}

/// 直接向 registry 注册 intercept 钩子，记录其执行（log_tag），放行不修改
fn record_intercept(reg: &mut HooksRegistry, priority: i32, log: &common::ExecLog, tag: &str) {
    let log = log.clone();
    let tag = tag.to_string();
    let handler: OutputInterceptFn = Arc::new(move |_msg| {
        log.lock().unwrap().push(tag.clone());
        None
    });
    reg.register_output_intercept(priority, handler);
}

/// 向 registry 注册返回阻止的 intercept 钩子（reason, 记录 log_tag）
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
        Some(reason.clone())
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
// 优先级排序（经 finalize 排定后）
// ============================================================================

#[test]
fn intercept_runs_in_priority_descending_after_finalize() {
    // 经 finalize 排定后，intercept 按 priority 降序执行。
    // 注册顺序故意与优先级相反（低优先级先注册），验证排序生效。
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let mut reg = new_registry();
    record_intercept(&mut reg, 1, &log, "low"); // priority=1
    record_intercept(&mut reg, 10, &log, "high"); // priority=10
    record_intercept(&mut reg, 5, &log, "mid"); // priority=5

    reg.finalize();
    reg.hook_output_intercept(&mut OutputEvent::Chunk(make_chunk(Some("e"), None)));

    let recorded = log.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec!["high", "mid", "low"],
        "finalize 后应按 priority 降序：high(10) → mid(5) → low(1)"
    );
}

// ============================================================================
// 冻结契约（隐藏契约，钉死）
// ============================================================================

#[test]
fn intercept_without_finalize_runs_in_registration_order() {
    // 隐藏契约：hook_output_intercept 不排序，finalize 是唯一排序点。
    // 不经 finalize 直接 intercept 时，执行顺序 = 注册顺序（非 priority 顺序）。
    // 钉死此契约：调用方必须先 finalize 才能享受优先级排序。
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let mut reg = new_registry();
    record_intercept(&mut reg, 1, &log, "low"); // 注册第 1
    record_intercept(&mut reg, 10, &log, "high"); // 注册第 2

    // 关键：不调 finalize，直接 intercept
    reg.hook_output_intercept(&mut OutputEvent::Chunk(make_chunk(Some("e"), None)));

    let recorded = log.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec!["low", "high"],
        "未经 finalize 时执行顺序 = 注册顺序（非 priority 顺序）"
    );
}

#[test]
fn finalize_sorts_once_subsequent_intercept_respects_priority() {
    // finalize 排定一次后 registry 冻结只读，重复 intercept 保持同样顺序（排序稳定）。
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let mut reg = new_registry();
    record_intercept(&mut reg, 1, &log, "low");
    record_intercept(&mut reg, 10, &log, "high");

    reg.finalize(); // 排序：high, low

    // 第一次 intercept 走排序后顺序
    reg.hook_output_intercept(&mut OutputEvent::Chunk(make_chunk(Some("e1"), None)));
    let first = log.lock().unwrap().clone();
    assert_eq!(first, vec!["high", "low"]);

    // 第二次 intercept 应保持同样的排序顺序（冻结后不重排）
    log.lock().unwrap().clear();
    reg.hook_output_intercept(&mut OutputEvent::Chunk(make_chunk(Some("e2"), None)));
    let second = log.lock().unwrap().clone();
    assert_eq!(
        second,
        vec!["high", "low"],
        "排序结果应稳定，不因重复 intercept 变化"
    );
}

// ============================================================================
// 阻止短路
// ============================================================================

#[test]
fn intercept_block_short_circuits_remaining_handlers() {
    // 任一 intercept 返回 Some(reason) 立即终止，后续 intercept 不执行。
    // 即使高优先级的 intercept 放行，中间一个阻止也应短路。
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let mut reg = new_registry();
    record_intercept(&mut reg, 10, &log, "high_pass"); // 放行
    block_intercept(&mut reg, 5, &log, "mid_block", "被阻止"); // priority=5 阻止
    record_intercept(&mut reg, 1, &log, "low_never"); // 不应执行

    reg.finalize(); // 排序：high_pass, mid_block, low_never

    let result = reg.hook_output_intercept(&mut OutputEvent::Chunk(make_chunk(Some("e"), None)));

    match result {
        Some(reason) => assert_eq!(reason, "被阻止"),
        None => panic!("应返回阻止原因"),
    }
    let recorded = log.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec!["high_pass", "mid_block"],
        "阻止后 low_never 不应执行"
    );
}

// ============================================================================
// intercept 原地串联修改
// ============================================================================

#[test]
fn intercept_chain_mutates_event_in_place_in_sequence() {
    // 多个放行的 intercept 链式原地修改同一事件：每个钩子看到前一个的修改结果。
    // 用 content 字段串联追加，验证原地修改的链式可见性。
    let mut reg = new_registry();
    // 低优先级钩子：content 追加 "-A"
    reg.register_output_intercept(
        1,
        Arc::new(|msg| {
            if let OutputEvent::Chunk(c) = msg
                && let Some(ref mut s) = c.payload.content
            {
                s.push_str("-A");
            }
            None
        }),
    );
    // 高优先级钩子：记录看到的 content（应看到原始内容，先于低优先级执行）
    let seen = Arc::new(Mutex::new(String::new()));
    let seen_clone = seen.clone();
    reg.register_output_intercept(
        2,
        Arc::new(move |msg| {
            if let OutputEvent::Chunk(c) = msg {
                *seen_clone.lock().unwrap() = c.payload.content.clone().unwrap_or_default();
            }
            None
        }),
    );

    reg.finalize(); // 排序：priority 2 先，1 后

    let mut ev = OutputEvent::Chunk(make_chunk(Some("base"), None));
    let result = reg.hook_output_intercept(&mut ev);

    // priority 2 先执行，此时 content 还没被 priority 1 追加
    let seen_by_high = seen.lock().unwrap().clone();
    assert_eq!(seen_by_high, "base", "高优先级先执行，此时应看到原始内容");

    // 最终结果：原地修改保留在事件上（priority 1 追加 "-A"），全放行返回 None
    assert!(result.is_none(), "全部放行应返回 None");
    if let OutputEvent::Chunk(c) = ev {
        assert_eq!(c.payload.content.as_deref(), Some("base-A"));
    }
}

// ============================================================================
// intercept panic 防护（公共 API 契约再钉一次）
// ============================================================================

#[test]
fn intercept_panic_does_not_block_subsequent_handlers() {
    // 单个 intercept panic 应被恢复，后续 intercept 仍执行，
    // 事件以 panic 时的当前值继续走完链路。
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let mut reg = new_registry();
    panic_intercept(&mut reg, 10, &log, "boom"); // 高优先级 panic
    record_intercept(&mut reg, 1, &log, "survivor"); // 应继续执行

    reg.finalize(); // 排序：boom, survivor

    // panic 被恢复，不应传播；survivor 仍执行
    let result = reg.hook_output_intercept(&mut OutputEvent::Chunk(make_chunk(Some("e"), None)));
    assert!(result.is_none(), "panic 恢复后应放行到 None");

    let recorded = log.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec!["boom", "survivor"],
        "boom panic 被恢复后 survivor 仍应执行"
    );
}

#[test]
fn intercept_all_panic_leaves_event_untouched() {
    // 所有 intercept 都 panic：全部被恢复，事件保持原值（无钩子完成过修改）
    let log: common::ExecLog = Arc::new(Mutex::new(Vec::new()));
    let mut reg = new_registry();
    panic_intercept(&mut reg, 5, &log, "boom1");
    panic_intercept(&mut reg, 3, &log, "boom2");

    reg.finalize();

    let mut ev = OutputEvent::Chunk(make_chunk(Some("untouched"), None));
    let result = reg.hook_output_intercept(&mut ev);
    assert!(result.is_none(), "全 panic 时应放行");
    if let OutputEvent::Chunk(c) = ev {
        assert_eq!(c.payload.content.as_deref(), Some("untouched"));
    }
}

// ============================================================================
// observe 与 intercept 互不干扰（语义正交）
// ============================================================================

#[tokio::test]
async fn observe_does_not_affect_intercept_result() {
    // observe 是只读副作用（共享 Arc 只读），intercept 是可修改/阻止。
    // 两者在同一 registry 共存时，observe 的执行不影响 intercept 的返回值（语义正交）。
    let observe_count = Arc::new(Mutex::new(0u32));
    let mut reg = new_registry();

    let count = observe_count.clone();
    reg.register_output_observe(
        0,
        Arc::new(move |_msg| {
            let count = count.clone();
            Box::pin(async move {
                *count.lock().unwrap() += 1;
            })
        }),
    );
    record_intercept(&mut reg, 0, &Arc::new(Mutex::new(vec![])), "noop");

    let mut event = OutputEvent::Chunk(make_chunk(Some("payload"), None));
    reg.hook_output_observe(Arc::new(event.clone())).await;
    let result = reg.hook_output_intercept(&mut event);

    assert_eq!(*observe_count.lock().unwrap(), 1, "observe 应执行一次");
    assert!(result.is_none());
    // content 不被 observe 修改
    if let OutputEvent::Chunk(c) = event {
        assert_eq!(c.payload.content.as_deref(), Some("payload"));
    }
}
