//! fuyao-session 集成测试：压缩触发器与反抖动状态机契约
//!
//! src/compressor/trigger.rs 的单元测试已覆盖 should_compress 的核心逻辑（用手动构造的 cfg）。
//! 本文件聚焦**公开 API 作为被消费契约的边界**——与单测的差异化视角：
//!
//! - CompressionConfig::default() 的默认阈值契约（单测用手动 cfg，未钉死默认值语义）
//! - CompressionRuntimeState 反抖动状态机的连续演进契约（record_compaction 链式调用）
//! - should_compress + 反抖动的组合决策（作为外部消费者看到的状态机行为）
//!
//! 注：apply / mark_compaction 依赖未导出的 SummaryResult / CompressionReason，
//! 属于内部实现细节，集成测试不直接调用（其端到端契约由 src/compressor/apply.rs 内单测覆盖）。

mod common;

use fuyao_api::CompressionConfig;
use fuyao_session::{CompressionRuntimeState as CompressionState, should_compress};

// common 模块的 unique_paths / temp_store 在纯逻辑测试中不被使用，
// 但 tests/common/mod.rs 作为跨二进制共享 fixture 需保持编译可见。

/// 默认 config（不调 set_config，直接构造默认值钉死其语义）
fn default_cfg() -> CompressionConfig {
    CompressionConfig::default()
}

// ============================================================================
// CompressionConfig::default 默认值契约
// ============================================================================

#[test]
fn default_config_is_enabled_with_reasonable_threshold() {
    // 钉死默认配置语义：默认开启、阈值 0.85、保留比例 0.05、反抖动 10%
    let cfg = default_cfg();
    assert!(cfg.enabled, "压缩默认应开启");
    assert!((cfg.threshold - 0.85).abs() < 1e-9, "默认阈值应为 0.85");
    assert!(
        (cfg.keep_ratio - 0.05).abs() < 1e-9,
        "默认保留比例应为 0.05"
    );
    assert_eq!(cfg.min_savings_pct, 10, "默认反抖动阈值应为 10%");
}

#[test]
fn default_config_does_not_trigger_on_small_context() {
    // 默认 config + 小上下文 + 少量 token → 不应触发压缩
    let cfg = default_cfg();
    let state = CompressionState::default();
    // default fallback_context = 128000，usable ≈ 123904，trigger_line ≈ 0.85 × 123904 ≈ 105318
    assert!(
        !should_compress(1_000, 128_000, &cfg, &state),
        "1k token 远低于阈值，不应触发"
    );
}

// ============================================================================
// 反抖动状态机连续演进契约
// ============================================================================

#[test]
fn antidebounce_state_starts_empty() {
    // 新会话的初始状态：无压缩历史（last/prev 都是 None）
    let state = CompressionState::default();
    assert!(state.last_savings.is_none());
    assert!(state.prev_savings.is_none());
}

#[test]
fn record_compaction_shifts_last_to_prev() {
    // 连续 record_compaction：last 滑入 prev，新值进 last（FIFO 滑动窗口）
    let mut state = CompressionState::default();

    // 第一次压缩：节省 50%
    state.record_compaction(100_000, 50_000);
    assert_eq!(state.last_savings, Some(0.5));
    assert!(state.prev_savings.is_none(), "首次压缩 prev 仍为 None");

    // 第二次压缩：节省 40%——last 滑入 prev
    state.record_compaction(100_000, 60_000);
    assert_eq!(state.last_savings, Some(0.4));
    assert_eq!(state.prev_savings, Some(0.5), "上次 last 应滑入 prev");
}

#[test]
fn antidebounce_blocks_after_two_consecutive_low_savings() {
    // 反抖动核心契约：连续两次节省 < min_savings_pct → 第三次即使超阈值也不压缩
    let cfg = default_cfg();
    let mut state = CompressionState::default();

    // 两次低效压缩（节省仅 5%，远低于默认 10% 阈值）
    state.record_compaction(100_000, 95_000); // 5%
    state.record_compaction(100_000, 95_000); // 5%

    // 即使 token 严重超阈值，反抖动也应阻止压缩
    assert!(
        !should_compress(150_000, 128_000, &cfg, &state),
        "连续两次低效压缩后应被反抖动阻止"
    );
}

#[test]
fn antidebounce_recovers_when_savings_improve() {
    // 反抖动恢复契约：低效压缩后若节省回升（单次 >= 阈值），不应被阻止
    let cfg = default_cfg();
    let mut state = CompressionState::default();

    // 一次低效（5%）+ 一次高效（50%）——prev=5%、last=50%，不满足「连续两次都低」
    state.record_compaction(100_000, 95_000); // 5%
    state.record_compaction(100_000, 50_000); // 50%

    assert!(
        should_compress(150_000, 128_000, &cfg, &state),
        "非连续低效（最后一次高效）不应被反抖动阻止"
    );
}

#[test]
fn antidebounce_never_blocks_before_two_records() {
    // 少于两次压缩记录时，反抖动条件不成立（需要 prev + last 都 Some）
    let cfg = default_cfg();
    let mut state = CompressionState::default();

    // 仅一次低效压缩（prev=None，反抖动不触发）
    state.record_compaction(100_000, 95_000); // 5%
    assert!(
        should_compress(150_000, 128_000, &cfg, &state),
        "仅一次压缩记录时反抖动不应阻止（需连续两次）"
    );
}

// ============================================================================
// should_compress 边界条件
// ============================================================================

#[test]
fn disabled_config_never_triggers_regardless_of_tokens() {
    // enabled=false 时，无论 token 多大都不触发
    let mut cfg = default_cfg();
    cfg.enabled = false;
    let state = CompressionState::default();
    assert!(!should_compress(u32::MAX, 128_000, &cfg, &state));
}

#[test]
fn zero_prompt_tokens_never_triggers() {
    // prompt_tokens=0 时（尚未有 LLM 调用）不触发
    let cfg = default_cfg();
    let state = CompressionState::default();
    assert!(!should_compress(0, 128_000, &cfg, &state));
}
