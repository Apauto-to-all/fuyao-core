//! 触发层：阈值检测 + 反抖动
//!
//! 触发公式：
//! ```text
//! prompt_tokens >= threshold × (context_length - summary_max_tokens)
//! ```
//! - `prompt_tokens` 来自上一轮 LLM 返回的真实 usage
//! - `context_length` 由调用方从 ModelConfig 解析后传入；解析不到用 cfg.fallback_context
//! - `summary_max_tokens` 作为输出预留扣除（防止压缩后又因输出超限触发）

use fuyao_api::CompressionConfig;

/// 压缩运行时状态（per-session，反抖动用）
///
/// 反抖动策略：记录上次压缩的节省比例，
/// 连续两次低于 `min_savings_pct` 时停压缩——避免无效循环。
#[derive(Debug, Default, Clone)]
pub struct CompressionState {
    /// 上一次压缩的节省比例（0.0~1.0），None 表示尚未压缩过
    pub last_savings: Option<f64>,
    /// 上上次压缩的节省比例（用于连续两次判定）
    pub prev_savings: Option<f64>,
}

impl CompressionState {
    /// 更新压缩历史（apply 层每次成功压缩后调用）
    pub fn record_compaction(&mut self, before_tokens: u32, after_tokens: u32) {
        let savings = if before_tokens == 0 {
            0.0
        } else {
            1.0 - (after_tokens as f64 / before_tokens as f64)
        };
        self.prev_savings = self.last_savings;
        self.last_savings = Some(savings);
    }
}

/// 阈值检测：pre-turn 时调，决定是否压缩
///
/// # 参数
/// - `prompt_tokens`：上一轮 LLM 返回的真实 prompt_tokens
/// - `context_length`：当前模型的上下文长度上限（由调用方从 ModelConfig 解析）
/// - `cfg`：压缩配置
/// - `state`：运行时状态（反抖动）
pub fn should_compress(
    prompt_tokens: u32,
    context_length: u32,
    cfg: &CompressionConfig,
    state: &CompressionState,
) -> bool {
    if !cfg.enabled || prompt_tokens == 0 {
        return false;
    }

    // 反抖动：连续两次节省比例低于阈值就停
    if let (Some(prev), Some(last)) = (state.prev_savings, state.last_savings) {
        let threshold = cfg.min_savings_pct as f64 / 100.0;
        if prev < threshold && last < threshold {
            tracing::warn!(
                prev_savings = prev,
                last_savings = last,
                min_savings_pct = cfg.min_savings_pct,
                "连续两次压缩效果不足，跳过本次压缩（反抖动）"
            );
            return false;
        }
    }

    // 阈值公式：prompt_tokens >= threshold × (context_length - summary_max_tokens)
    let usable = context_length.saturating_sub(cfg.summary_max_tokens as u32);
    let trigger_line = (cfg.threshold * usable as f64) as u32;
    prompt_tokens >= trigger_line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CompressionConfig {
        CompressionConfig {
            enabled: true,
            threshold: 0.85,
            keep_ratio: 0.05,
            keep_tokens_max: 8000,
            summary_max_tokens: 4096,
            fallback_context: 128_000,
            min_savings_pct: 10,
            tokens_per_image: 1000,
            skip_child: true,
        }
    }

    #[test]
    fn disabled_never_triggers() {
        let mut c = cfg();
        c.enabled = false;
        let s = CompressionState::default();
        assert!(!should_compress(999_999, 100_000, &c, &s));
    }

    #[test]
    fn zero_tokens_never_triggers() {
        let c = cfg();
        let s = CompressionState::default();
        assert!(!should_compress(0, 100_000, &c, &s));
    }

    #[test]
    fn triggers_when_above_threshold() {
        // context=128000, summary=4096, usable=123904, 0.85×123904 ≈ 105318
        let c = cfg();
        let s = CompressionState::default();
        assert!(should_compress(110_000, 128_000, &c, &s));
    }

    #[test]
    fn no_trigger_when_below_threshold() {
        let c = cfg();
        let s = CompressionState::default();
        assert!(!should_compress(50_000, 128_000, &c, &s));
    }

    #[test]
    fn anti_bounce_stops_after_two_low_savings() {
        let c = cfg();
        let mut s = CompressionState::default();
        // 连续两次节省仅 5%（< 10% 阈值）
        s.record_compaction(100_000, 95_000); // savings = 5%
        s.record_compaction(100_000, 95_000); // savings = 5%
        assert!(!should_compress(110_000, 128_000, &c, &s));
    }

    #[test]
    fn anti_bounce_allows_when_savings_above_threshold() {
        let c = cfg();
        let mut s = CompressionState::default();
        s.record_compaction(100_000, 50_000); // savings = 50%
        s.record_compaction(100_000, 60_000); // savings = 40%
        assert!(should_compress(110_000, 128_000, &c, &s));
    }

    #[test]
    fn record_compaction_handles_zero_before() {
        let mut s = CompressionState::default();
        s.record_compaction(0, 0);
        assert_eq!(s.last_savings, Some(0.0));
    }
}
