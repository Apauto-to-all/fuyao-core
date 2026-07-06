//! Agent 运行时类型定义

use crate::agent::AgentPaths;
use crate::provider::ThinkingType;
use std::sync::Arc;

/// 共享 Agent 上下文
pub type SharedAgentCtx = Arc<std::sync::Mutex<AgentContext>>;

/// 模型运行配置
///
/// 聚合「用哪个模型」+「怎么思考」三个运行时模型参数。
/// 三字段默认 `None`，请求体不发对应字段，走模型自身默认行为。
#[derive(Debug, Clone, Default)]
pub struct ModelConfig {
    /// 模型 ID，如 aliyun/qwen3.6-plus。None 时自动使用默认模型
    pub model_id: Option<String>,

    /// 思考开关（对应 thinking.type 字段）。None 时不发，走模型默认
    pub thinking_type: Option<ThinkingType>,

    /// 思考强度档位名（用户自定义字符串，透传给服务器）。None 时不发，走模型默认
    pub reasoning_effort: Option<String>,
}

/// Agent 运行上下文
#[derive(Debug, Clone, Default)]
pub struct AgentContext {
    /// 模型运行配置（聚合模型选择 + 思考控制）
    pub model_config: ModelConfig,

    /// 要加载的 Session ID，None 时创建新 Session
    pub session_id: Option<String>,

    /// Agent 三层目录的身份证明，默认使用全局层
    pub agent_paths: AgentPaths,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_context_default_has_no_ids() {
        let ctx = AgentContext::default();
        assert!(ctx.model_config.model_id.is_none());
        assert!(ctx.session_id.is_none());
        assert!(ctx.agent_paths.agent_id.is_none());
    }

    #[test]
    fn model_config_default_all_none() {
        let config = ModelConfig::default();
        assert!(config.model_id.is_none());
        assert!(config.thinking_type.is_none());
        assert!(config.reasoning_effort.is_none());
    }
}
