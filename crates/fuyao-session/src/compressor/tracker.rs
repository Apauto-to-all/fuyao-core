//! 压缩触发追踪器
//!
//! 简化的阈值检测器：只在助手消息到达时检测一次。
//! 引导消息走引擎 Guide 队列，引擎自动处理完当前工具调用后再消费，
//! 无需手动跟踪工具结果到齐状态。

use fuyao_api::AgentPaths;

use crate::utils::resolve_context_length;

/// 压缩触发阈值：上下文使用率（prompt_tokens / context_length）达到此比例时触发
const COMPRESSION_THRESHOLD: f64 = 0.85;

/// 压缩触发追踪器 — 阈值检测
///
/// 引导消息走 Guide 队列，引擎自动处理工具调用后再消费，无需 pending 状态机。
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
    pub fn should_compress(&self, prompt_tokens: usize) -> bool {
        if prompt_tokens == 0 {
            return false;
        }
        let context_length = self
            .agent_paths
            .as_ref()
            .map(resolve_context_length)
            .unwrap_or(128_000);
        prompt_tokens as f64 >= context_length as f64 * COMPRESSION_THRESHOLD
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_new_is_clean() {
        let t = CompressionTracker::new();
        assert!(!t.should_compress(0));
    }

    #[test]
    fn tracker_no_compress_below_threshold() {
        let t = CompressionTracker::new();
        // 默认 context_length = 128000, threshold = 0.85
        // 100000 / 128000 = 0.78 < 0.85
        assert!(!t.should_compress(100_000));
    }

    #[test]
    fn tracker_compress_above_threshold() {
        let t = CompressionTracker::new();
        // 110000 / 128000 = 0.86 > 0.85
        assert!(t.should_compress(110_000));
    }

    #[test]
    fn tracker_zero_tokens_no_compress() {
        let t = CompressionTracker::new();
        assert!(!t.should_compress(0));
    }

    #[test]
    fn tracker_exact_threshold_triggers() {
        let t = CompressionTracker::new();
        // 128000 * 0.85 = 108800
        assert!(t.should_compress(108_800));
    }

    #[test]
    fn tracker_custom_agent_paths() {
        let mut t = CompressionTracker::new();
        // 用不存在的 agent_id，resolve_context_length 回退到默认 128000
        t.set_agent_paths(AgentPaths {
            agent_id: Some("test".to_string()),
            workspace: None,
        });
        assert!(t.should_compress(110_000));
    }
}
