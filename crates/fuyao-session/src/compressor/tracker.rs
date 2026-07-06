//! 压缩触发追踪器
//!
//! 简化的阈值检测器：只在助手消息到达时检测一次。
//! 引导消息走引擎 Guide 队列，引擎自动处理完当前工具调用后再消费，
//! 无需手动跟踪工具结果到齐状态。

use fuyao_api::AgentPaths;

use crate::utils::resolve_context_length;

/// 压缩触发追踪器 — 阈值检测
///
/// 引导消息走 Guide 队列，引擎自动处理工具调用后再消费，无需 pending 状态机。
/// 阈值与 fallback 从全局配置 `get_config().session.compression` 读取。
pub struct CompressionTracker {
    /// Agent 路径配置（用于动态获取 context_length）
    agent_paths: Option<AgentPaths>,
}

impl Default for CompressionTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CompressionTracker {
    /// 创建新的追踪器
    pub fn new() -> Self {
        Self { agent_paths: None }
    }

    /// 设置 Agent 路径配置
    pub fn set_agent_paths(&mut self, agent_paths: AgentPaths) {
        self.agent_paths = Some(agent_paths);
    }

    /// 检测是否需要压缩：prompt_tokens >= context_length × threshold
    ///
    /// 在 output_observe(Assistant) 时调用，无论是否有工具调用。
    /// 引导消息走 Guide 队列，引擎自动在当前工具调用完成后消费。
    ///
    /// # Arguments
    /// * `prompt_tokens` - 当前 prompt token 数
    /// * `model_id` - 当前运行时模型 ID（从 agent_ctx 读取），用于解析 context_length
    pub fn should_compress(&self, prompt_tokens: usize, model_id: Option<&str>) -> bool {
        if prompt_tokens == 0 {
            return false;
        }
        let cfg = fuyao_api::get_config();
        let compression = &cfg.session.compression;
        let context_length = self
            .agent_paths
            .as_ref()
            .map(|p| resolve_context_length(model_id, p))
            .unwrap_or(compression.fallback_context as usize);
        prompt_tokens as f64 >= context_length as f64 * compression.threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_new_is_clean() {
        let t = CompressionTracker::new();
        assert!(!t.should_compress(0, None));
    }

    #[test]
    fn tracker_no_compress_below_threshold() {
        let t = CompressionTracker::new();
        // 默认 context_length(fallback) = 128000, threshold = 0.85
        // 100000 / 128000 = 0.78 < 0.85
        assert!(!t.should_compress(100_000, None));
    }

    #[test]
    fn tracker_compress_above_threshold() {
        let t = CompressionTracker::new();
        // 110000 / 128000 = 0.86 > 0.85
        assert!(t.should_compress(110_000, None));
    }

    #[test]
    fn tracker_zero_tokens_no_compress() {
        let t = CompressionTracker::new();
        assert!(!t.should_compress(0, None));
    }

    #[test]
    fn tracker_exact_threshold_triggers() {
        let t = CompressionTracker::new();
        // 128000 * 0.85 = 108800
        assert!(t.should_compress(108_800, None));
    }

    #[test]
    fn tracker_custom_agent_paths() {
        let mut t = CompressionTracker::new();
        // 用不存在的 agent_id，resolve_context_length 回退到默认 fallback 128000
        t.set_agent_paths(AgentPaths {
            agent_id: Some("test".to_string()),
            workspace: None,
        });
        // 传一个不存在的 model_id，get_model 查不到 → 回退 fallback
        assert!(t.should_compress(110_000, Some("nonexistent/model")));
    }
}
