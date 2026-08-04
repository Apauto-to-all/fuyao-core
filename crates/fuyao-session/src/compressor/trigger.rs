//! 触发层：阈值检测
//!
//! 触发公式：
//! ```text
//! prompt_tokens >= threshold × (context_length - summary_max_tokens)
//! ```
//! - `prompt_tokens` 来自上一轮 LLM 返回的真实 usage
//! - `context_length` 由调用方从 ModelConfig 解析后传入；解析不到用 cfg.fallback_context
//! - `summary_max_tokens` 作为输出预留扣除（防止压缩后又因输出超限触发）

use fuyao_api::CompressionConfig;

/// 阈值检测：pre-turn 时调，决定是否压缩
///
/// # 参数
/// - `prompt_tokens`：上一轮 LLM 返回的真实 prompt_tokens
/// - `context_length`：当前模型的上下文长度上限（由调用方从 ModelConfig 解析）
/// - `cfg`：压缩配置
pub fn should_compress(prompt_tokens: u32, context_length: u32, cfg: &CompressionConfig) -> bool {
    if !cfg.enabled || prompt_tokens == 0 {
        return false;
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
            tokens_per_image: 1000,
            skip_child: true,
        }
    }

    #[test]
    fn disabled_never_triggers() {
        let mut c = cfg();
        c.enabled = false;
        assert!(!should_compress(999_999, 100_000, &c));
    }

    #[test]
    fn zero_tokens_never_triggers() {
        let c = cfg();
        assert!(!should_compress(0, 100_000, &c));
    }

    #[test]
    fn triggers_when_above_threshold() {
        // context=128000, summary=4096, usable=123904, 0.85×123904 ≈ 105318
        let c = cfg();
        assert!(should_compress(110_000, 128_000, &c));
    }

    #[test]
    fn no_trigger_when_below_threshold() {
        let c = cfg();
        assert!(!should_compress(50_000, 128_000, &c));
    }
}
