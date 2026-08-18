//! fuyao-session 集成测试：压缩触发器契约
//!
//! src/compressor/trigger.rs 的单元测试已覆盖 should_compress 的核心逻辑（用手动构造的 cfg）。
//! 本文件聚焦**公开 API 作为被消费契约的边界**——与单测的差异化视角：
//!
//! - CompressionConfig::default() 的默认阈值契约（单测用手动 cfg，未钉死默认值语义）
//! - should_compress 的阈值门决策（作为外部消费者看到的行为）
//!
//! 注：apply / mark_compaction 依赖未导出的 SummaryResult / CompressionReason，
//! 属于内部实现细节，集成测试不直接调用（其端到端契约由 src/compressor/apply.rs 内单测覆盖）。

mod common;

use fuyao_api::CompressionConfig;
use fuyao_session::should_compress;

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
    // 钉死默认配置语义：默认开启、阈值 0.85
    let cfg = default_cfg();
    assert!(cfg.enabled, "压缩默认应开启");
    assert!((cfg.threshold - 0.85).abs() < 1e-9, "默认阈值应为 0.85");
}

#[test]
fn default_config_does_not_trigger_on_small_context() {
    // 默认 config + 小上下文 + 少量 token → 不应触发压缩
    let cfg = default_cfg();
    // context=128000，usable ≈ 123904，trigger_line ≈ 0.85 × 123904 ≈ 105318
    assert!(
        !should_compress(1_000, 128_000, &cfg),
        "1k token 远低于阈值，不应触发"
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
    assert!(!should_compress(u32::MAX, 128_000, &cfg));
}

#[test]
fn zero_prompt_tokens_never_triggers() {
    // prompt_tokens=0 时（尚未有 LLM 调用）不触发
    let cfg = default_cfg();
    assert!(!should_compress(0, 128_000, &cfg));
}

#[test]
fn triggers_when_above_threshold() {
    // 默认 config + 远超阈值 → 应触发
    let cfg = default_cfg();
    assert!(
        should_compress(150_000, 128_000, &cfg),
        "150k token 超过阈值，应触发"
    );
}
